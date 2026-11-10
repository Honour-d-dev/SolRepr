use crate::hir::analysis::Analysis;
use crate::hir::diagnostics::SemanticDiagnostic;
use crate::ir::def_map::*;
use crate::salsa::Db;
use crate::salsa::root_db::RootDb;
use crate::workspace::{LoadedFile, Package as WsPackage, PackageId, SourceRoot, Workspace};
use camino::Utf8PathBuf;
use ropey::Rope;

const ROOT: &str = "/tmp/def_map_tests";

fn db_with(files: &[(&str, &str)]) -> Db {
    let root = Utf8PathBuf::from(ROOT);
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
            files: files
                .iter()
                .map(|(path, text)| LoadedFile {
                    path: root.join(path),
                    text: Rope::from_str(text),
                })
                .collect(),
        }],
    };
    Db::new(workspace)
}

fn analysis_of(db: &Db) -> Analysis<DefMap> {
    let file = db
        .file(&Utf8PathBuf::from(format!("{ROOT}/src/Test.sol")))
        .unwrap();
    db.root_def_map_analysis(RootDb::file_source_root(db, file))
}

fn scope_of(defmap: &DefMap, contract: &str) -> ScopeId {
    defmap
        .defs
        .iter()
        .find(|(id, data)| {
            matches!(id, DefId::Contract(_)) && data.name.as_deref() == Some(contract)
        })
        .and_then(|(_, data)| data.child_scope)
        .expect("contract scope")
}

/// Every def declared under `name` in `scope`, lookup-visible or not.
fn declared_in(defmap: &DefMap, scope: ScopeId, name: &str) -> Vec<DefId> {
    defmap
        .defs
        .iter()
        .filter(|(_, data)| data.scope == scope && data.name.as_deref() == Some(name))
        .map(|(id, _)| *id)
        .collect()
}

#[test]
fn rejected_declarations_are_retained_but_not_lookup_candidates() {
    let db = db_with(&[(
        "src/Test.sol",
        "contract C { uint x; uint x; function f() external {} uint f; modifier m() {_;} modifier m() {_;} struct S { uint a; } struct S { uint b; } }",
    )]);
    let analysis = analysis_of(&db);
    let scope = scope_of(&analysis.data, "C");

    for name in ["x", "f", "m", "S"] {
        let declared = declared_in(&analysis.data, scope, name);
        let winners = analysis.data.scopes[scope].by_name[name].defs.clone();
        assert_eq!(declared.len(), 2, "{name}: both declarations retained");
        assert_eq!(winners.len(), 1, "{name}: one lookup candidate");
        assert_eq!(
            declared.iter().filter(|id| !winners.contains(id)).count(),
            1,
            "{name}: the loser is kept out of lookup"
        );
    }

    let redeclarations: Vec<_> = analysis
        .diagnostics
        .iter()
        .filter_map(|d| match d {
            SemanticDiagnostic::Redeclaration {
                rejected, previous, ..
            } => Some((*rejected, *previous)),
            _ => None,
        })
        .collect();
    assert_eq!(redeclarations.len(), 4);
    for (rejected, previous) in redeclarations {
        assert!(
            !analysis.data.scopes[scope]
                .by_name
                .values()
                .any(|sd| sd.defs.contains(&rejected))
        );
        assert!(
            analysis.data.scopes[scope]
                .by_name
                .values()
                .any(|sd| sd.defs.contains(&previous))
        );
    }
}

#[test]
fn overloads_are_not_rejected() {
    let db = db_with(&[(
        "src/Test.sol",
        "contract C { function f(uint) external {} function f(bool) external {} }",
    )]);
    let analysis = analysis_of(&db);

    assert!(analysis.diagnostics.is_empty());
    assert_eq!(
        analysis.data.scopes[scope_of(&analysis.data, "C")].by_name["f"]
            .defs
            .len(),
        2
    );
}

#[test]
fn import_collisions_and_failures_are_reported_at_the_directive() {
    let db = db_with(&[
        ("src/A.sol", "contract A {} contract B {}"),
        (
            "src/Test.sol",
            "import {A} from \"./A.sol\";\ncontract A {}\nimport {Missing} from \"./A.sol\";\nimport \"./Nowhere.sol\";\n",
        ),
    ]);
    let analysis = analysis_of(&db);

    let mut collision = false;
    let mut missing = false;
    let mut unresolved = false;
    for diagnostic in analysis.diagnostics.iter() {
        match diagnostic {
            SemanticDiagnostic::Redeclaration {
                rejected: DefId::Import(_),
                previous: DefId::Contract(_),
                ..
            } => collision = true,
            SemanticDiagnostic::ImportSymbolNotFound { name, path, .. } => {
                missing = name == "Missing" && path == "./A.sol"
            }
            SemanticDiagnostic::UnresolvedImport { path, .. } => {
                unresolved = path == "./Nowhere.sol"
            }
            other => panic!("unexpected diagnostic: {:?}", other.code()),
        }
    }
    let codes: Vec<_> = analysis.diagnostics.iter().map(|d| d.code()).collect();
    assert!(
        collision && missing && unresolved,
        "{codes:?} {collision} {missing} {unresolved}"
    );
    assert_eq!(analysis.diagnostics.len(), 3);
}

#[test]
fn namespace_alias_collisions_are_reported() {
    let db = db_with(&[
        ("src/A.sol", "contract A {}"),
        ("src/B.sol", "contract B {}"),
        (
            "src/Test.sol",
            "import \"./A.sol\" as Q;\nimport \"./B.sol\" as Q;\n",
        ),
    ]);
    let analysis = analysis_of(&db);

    assert!(matches!(
        analysis.diagnostics.as_ref(),
        [SemanticDiagnostic::Redeclaration {
            rejected: DefId::Import(_),
            previous: DefId::File(_),
            ..
        }]
    ));
}

#[test]
fn legal_import_cycles_report_nothing() {
    let db = db_with(&[
        ("src/A.sol", "import {Y} from \"./B.sol\";\ncontract X {}"),
        ("src/B.sol", "import {X} from \"./A.sol\";\ncontract Y {}"),
        (
            "src/Test.sol",
            "import {X} from \"./A.sol\";\ncontract C {}",
        ),
    ]);
    let analysis = analysis_of(&db);

    let codes: Vec<_> = analysis.diagnostics.iter().map(|d| d.code()).collect();
    assert!(analysis.diagnostics.is_empty(), "{codes:?}");
}

#[test]
fn duplicate_special_functions_are_reported() {
    let db = db_with(&[(
        "src/Test.sol",
        "contract C { constructor() {} constructor(uint x) {} fallback() external {} fallback() external {} receive() external payable {} receive() external payable {} }",
    )]);
    let analysis = analysis_of(&db);
    let scope = scope_of(&analysis.data, "C");

    let reported = analysis
        .diagnostics
        .iter()
        .filter(|d| matches!(d, SemanticDiagnostic::Redeclaration { .. }))
        .count();
    assert_eq!(reported, 3);

    // Each special name keeps exactly one lookup candidate.
    for name in ["constructor", "fallback", "receive"] {
        assert_eq!(
            analysis.data.scopes[scope].by_name[name].defs.len(),
            1,
            "{name}"
        );
    }
}

#[test]
fn one_of_each_special_function_is_fine() {
    let db = db_with(&[(
        "src/Test.sol",
        "contract C { constructor() {} fallback() external {} receive() external payable {} }",
    )]);
    let analysis = analysis_of(&db);
    assert!(analysis.diagnostics.is_empty());
}

#[test]
fn narrow_sums_admit_exactly_their_def_kinds() {
    let db = db_with(&[(
        "src/Test.sol",
        "type U is uint256;
         struct S { uint256 a; }
         enum E { A }
         error Er();
         event Ev();
         contract C { uint256 v; function f() public {} modifier m() { _; } }",
    )]);
    let analysis = analysis_of(&db);

    for &def in analysis.data.defs.keys() {
        assert_eq!(
            TypeDefId::try_from(def).is_ok(),
            matches!(
                def,
                DefId::Contract(_)
                    | DefId::Interface(_)
                    | DefId::Library(_)
                    | DefId::Struct(_)
                    | DefId::Enum(_)
                    | DefId::Udvt(_)
            ),
            "TypeDefId membership is wrong for {def:?}"
        );
        assert_eq!(
            CallableDefId::try_from(def).is_ok(),
            matches!(
                def,
                DefId::Function(_)
                    | DefId::Var(_)
                    | DefId::Contract(_)
                    | DefId::Error(_)
                    | DefId::Event(_)
                    | DefId::Modifier(_)
            ),
            "CallableDefId membership is wrong for {def:?}"
        );

        // Widening a narrowed id gives back the def it came from.
        if let Ok(id) = TypeDefId::try_from(def) {
            assert_eq!(DefId::from(id), def);
        }
        if let Ok(id) = CallableDefId::try_from(def) {
            assert_eq!(DefId::from(id), def);
        }
        // A typed id lifts to the same def the def map stores.
        if let DefId::Function(f) = def {
            assert_eq!(DefId::from(f), def);
        }
    }
}
