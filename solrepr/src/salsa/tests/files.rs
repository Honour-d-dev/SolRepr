use crate::paths::NormalizedPath;
use crate::salsa::root_db::{import_map, imports, imports_closure};
use crate::salsa::{Db, File};
use crate::utilities::fixture_path;
use crate::workspace::loader::load_roots;
use crate::workspace::{LoadedFile, Package, PackageId, RootWithFiles, WorkspaceWithFiles};
use camino::Utf8PathBuf;
use ropey::Rope;
use rustc_hash::FxHashSet;
use smol_str::SmolStr;

fn standalone_db() -> Db {
    let root = fixture_path("fixtures/workspaces/standalone/basic");
    Db::new(load_roots(root))
}

/// A stray document still attaches when a source root encloses it —
/// here the frameworkless container root covers the whole tree.
#[test]
fn stray_file_under_a_source_root_attaches_to_that_root() {
    let root = fixture_path("fixtures/workspaces/foundry/container");
    let mut db = Db::new(load_roots(root.clone()));
    let path = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/workspaces/foundry/container/a/b/inner/stray.sol");

    let file = db
        .open_file(path.clone(), "contract Stray {}".to_string())
        .expect("a file under a source root attaches");
    let source_root = db.file_source_root(file);
    assert_eq!(source_root.root(&db).as_path(), root.as_path());
}

/// A stray file inside a package root but outside every source root is
/// not indexed — the package's declared layout defines the compilation
/// unit, same as solc.
#[test]
fn stray_file_outside_source_dirs_is_not_indexed() {
    let root = fixture_path("fixtures/workspaces/foundry/nested");
    let mut db = Db::new(load_roots(root));
    let path = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/workspaces/foundry/nested/stray.sol");

    assert!(
        db.open_file(path.clone(), "contract Stray {}".to_string())
            .is_none()
    );
    assert!(db.file(&path).is_none());
}

/// A document outside every source root is not part of the compilation
/// unit: no `File`, no source root.
#[test]
fn file_outside_every_source_root_is_not_indexed() {
    let mut db = standalone_db();
    let path =
        Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/workspaces/AdHocScratch.sol");

    assert!(
        db.open_file(path.clone(), "contract AdHocScratch {}".to_string())
            .is_none()
    );
    assert!(db.file(&path).is_none());
}

/// A `remappings.txt`-only workspace resolves prefix imports into the
/// vendored directory scanned under its root source root.
#[test]
fn remappings_only_workspace_resolves_vendored_imports() {
    let fixture = fixture_path("fixtures/workspaces/foundry/remappings/vendor");
    let db = Db::new(load_roots(fixture.clone()));
    let main = db
        .file(&fixture.join("Main.sol"))
        .expect("the workspace indexes Main.sol");
    let dep = db
        .resolve_to_file(main, "dep/Dep.sol")
        .expect("dep/ resolves through remappings.txt");
    assert!(dep.path(&db).ends_with("vendor/dep/Dep.sol"));
}

/// A namespace import (`import * as ns`) is a qualifier, not a type: `ns.D`,
/// `ns.Pair` and the base list `is ns.D` all resolve through the imported
/// file's scope, in type position and in expression position (`ns.D(who)`).
#[test]
fn namespace_import_qualifies_type_references() {
    let root = fixture_path("fixtures/workspaces/namespaces");
    let db = Db::new(load_roots(root.clone()));
    let user = db
        .file(&root.join("src/User.sol"))
        .expect("the workspace indexes User.sol");

    let diagnostics = crate::hir::diagnostics::collect_diagnostics(&db, user);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

/// A namespace alias is writable but cannot type a declaration: solc 5172,
/// "Name has to refer to a user-defined type", once per declaration — and
/// nothing cascades, because the type is refused before anything binds it.
#[test]
fn namespace_alias_cannot_type_a_declaration() {
    let root = fixture_path("fixtures/workspaces/namespaces");
    let db = Db::new(load_roots(root.clone()));
    let bad = db
        .file(&root.join("src/Bad.sol"))
        .expect("the workspace indexes Bad.sol");

    let diagnostics = crate::hir::diagnostics::collect_diagnostics(&db, bad);
    let refusals = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("user-defined type"))
        .count();
    // `d`, the return type of `f`, `x`, `y`, and the element of `ns[] z`.
    assert_eq!(refusals, 5, "one per declaration: {diagnostics:?}");
    assert_eq!(diagnostics.len(), 5, "{diagnostics:?}");
}

/// The refusal is the *type ref's* verdict, reached while lowering, so a local,
/// its parameter and an array of it all end up invalid — no `Base.sol` survives
/// into a declaration and no location error follows from the refused type.
#[test]
fn a_refused_namespace_type_leaves_no_namespace_behind() {
    use crate::hir::body_map::BodyOwnerId;
    use crate::hir::tests::helper::{find_function, named_function_inference};
    use crate::ir::def_map::DefId;
    use crate::salsa::hir_db::HirDb;

    let root = fixture_path("fixtures/workspaces/namespaces");
    let db = Db::new(load_roots(root.clone()));
    let bad = db
        .file(&root.join("src/Bad.sol"))
        .expect("the workspace indexes Bad.sol");

    for name in ["local", "param", "array"] {
        let DefId::Function(id) = find_function(&db, bad, name) else {
            panic!("`{name}` should be a function");
        };
        let inference = named_function_inference(&db, bad, name);
        let body = db.body_map(BodyOwnerId::Function(id));
        for (local, data) in body.locals.iter() {
            let ty_ref = *data.ty_ref();
            assert!(
                inference.local_ty(local).is_invalid(&db),
                "`{name}`: the local kept {}",
                inference.local_ty(local).display(&db)
            );
            assert!(
                inference.ty(ty_ref).is_invalid(&db),
                "`{name}`: the type ref kept {}",
                inference.ty(ty_ref).display(&db)
            );
        }
    }
}

/// A manifestless vendored library is indexed as a named dependency —
/// the `lib` edge is trusted — and the auto-detected library remapping
/// resolves imports into it.
#[test]
fn manifestless_library_resolves_through_the_synthesized_remapping() {
    let fixture = fixture_path("fixtures/workspaces/foundry/nested");
    let db = Db::new(load_roots(fixture.clone()));
    let main = db
        .file(&fixture.join("src/Root.sol"))
        .expect("the workspace indexes src/Root.sol");
    let dep = db
        .resolve_to_file(main, "plain/Plain.sol")
        .expect("plain/ resolves into the named dependency");
    assert!(dep.path(&db).ends_with("dependencies/plain/Plain.sol"));
}

/// A remapping into a library's `src` dir resolves after the target is
/// normalized to its package root.
#[test]
fn remapping_into_src_dir_resolves_imports() {
    let fixture = fixture_path("fixtures/workspaces/foundry/nested");
    let db = Db::new(load_roots(fixture.clone()));
    let main = db
        .file(&fixture.join("src/Root.sol"))
        .expect("the workspace indexes src/Root.sol");
    let dep = db
        .resolve_to_file(main, "fs/Lib.sol")
        .expect("fs/ resolves into the library's src dir");
    assert!(dep.path(&db).ends_with("dependencies/fs/src/Lib.sol"));
}

#[test]
fn deduplicated_library_files_are_never_indexed() {
    let root = fixture_path("fixtures/workspaces/foundry/nested");
    let mut db = Db::new(load_roots(root));
    let path = fixture_path(
        "fixtures/workspaces/foundry/nested/dependencies/dep-a/lib/duplicate/src/Duplicate.sol",
    );

    assert!(db.is_ignored_path(&path));
    assert!(
        db.open_file(path.clone(), "contract OpenedDuplicate {}".to_string())
            .is_none(),
        "an ignored file gets no File and no source root"
    );
    assert!(db.file(&path).is_none());
}

/// A dependency's dev dirs are outside the compilation unit: files there
/// never attach — even though the enclosing workspace root's own source
/// root covers the whole tree.
#[test]
fn dependency_dev_files_are_never_indexed() {
    let root = fixture_path("fixtures/workspaces/foundry/dev_dirs");
    let path = root.join("lib/dep/test/Dep.t.sol");
    let mut db = Db::new(load_roots(root));

    assert!(db.is_ignored_path(&path));
    assert!(
        db.open_file(path.clone(), "contract T {}".to_string())
            .is_none(),
        "a dependency dev file gets no File and no source root"
    );
    assert!(db.file(&path).is_none());
}

/// The live root set must equal a fresh discovery of the same tree, otherwise
/// every watcher event produces a phantom diff.
#[test]
fn live_root_set_matches_a_fresh_discovery() {
    let fixtures = [
        "fixtures/workspaces/foundry/nested",
        "fixtures/workspaces/foundry/remappings",
        "fixtures/workspaces/foundry/dev_dirs",
        "fixtures/workspaces/hardhat/imports",
        "fixtures/workspaces/hybrid",
        "fixtures/workspaces/standalone/npm_workspace",
    ];
    for fixture in fixtures {
        let root = fixture_path(fixture);
        let db = Db::new(load_roots(root.clone()));
        let discovered = crate::workspace::Workspace::<()>::from(
            &crate::workspace::discover_workspaces(std::slice::from_ref(&root)),
        );
        assert_eq!(db.workspace(), discovered, "{fixture}");
    }
}

/// A non-relative import resolves against the package root as well as the
/// remapping table — Foundry's base path, which is how `import "src/Foo.sol"`
/// from a test file finds its target.
#[test]
fn a_project_rooted_import_resolves_against_the_package_root() {
    let root = Utf8PathBuf::from("/tmp/project_rooted_import");
    let package = PackageId(NormalizedPath::new(root.clone()));
    let workspace = WorkspaceWithFiles {
        ignored_roots: Vec::new(),
        packages: vec![Package {
            root: NormalizedPath::new(root.clone()),
            remappings: Vec::new(),
        }],
        roots: vec![
            RootWithFiles {
                path: NormalizedPath::new(root.join("src")),
                package_id: package.clone(),
                is_dependency: false,
                files: vec![LoadedFile {
                    path: NormalizedPath::new(root.join("src/Foo.sol")),
                    text: Some(Rope::from_str("contract Foo {}")),
                }],
            },
            RootWithFiles {
                path: NormalizedPath::new(root.join("test")),
                package_id: package,
                is_dependency: false,
                files: vec![LoadedFile {
                    path: NormalizedPath::new(root.join("test/Foo.t.sol")),
                    text: Some(Rope::from_str("import \"src/Foo.sol\";")),
                }],
            },
        ],
    };
    let db = Db::new(workspace);
    let from = db
        .file(&root.join("test/Foo.t.sol"))
        .expect("test file is indexed");
    let resolved = db.resolve_to_file(from, "src/Foo.sol");
    assert_eq!(
        resolved.map(|file| file.path(&db).clone()),
        Some(NormalizedPath::new(root.join("src/Foo.sol")))
    );
}

fn indexed(db: &Db, path: Utf8PathBuf) -> File {
    db.file(&path)
        .unwrap_or_else(|| panic!("{path} is indexed"))
}

/// One root, one package, files given as text — the smallest workspace that
/// exercises resolution without a tree on disk.
fn workspace(root: &str, files: &[(&str, &str)]) -> WorkspaceWithFiles<LoadedFile> {
    let root = NormalizedPath::new(root);
    let package = PackageId(root.clone());
    WorkspaceWithFiles {
        ignored_roots: Vec::new(),
        packages: vec![Package {
            root: root.clone(),
            remappings: Vec::new(),
        }],
        roots: vec![RootWithFiles {
            path: root.clone(),
            package_id: package,
            is_dependency: false,
            files: files
                .iter()
                .map(|(relative, text)| LoadedFile {
                    path: NormalizedPath::new(root.join(relative)),
                    text: Some(Rope::from_str(text)),
                })
                .collect(),
        }],
    }
}

#[test]
fn imports_resolve_through_the_remapping_table() {
    let fixture = fixture_path("fixtures/workspaces/foundry/remappings/vendor");
    let db = Db::new(load_roots(fixture.clone()));
    let main = indexed(&db, fixture.join("Main.sol"));
    let dep = indexed(&db, fixture.join("vendor/dep/Dep.sol"));

    let edges = imports(&db, main);
    assert_eq!(edges.resolved, vec![dep]);
    assert!(
        edges.unresolved.is_empty(),
        "every import in the fixture resolves"
    );
}

#[test]
fn importers_invert_the_table() {
    let fixture = fixture_path("fixtures/workspaces/foundry/remappings/vendor");
    let db = Db::new(load_roots(fixture.clone()));
    let main = indexed(&db, fixture.join("Main.sol"));
    let dep = indexed(&db, fixture.join("vendor/dep/Dep.sol"));

    let map = import_map(&db);
    assert_eq!(map.of(dep), &[main]);
    assert_eq!(map.dependents_of([dep]), FxHashSet::from_iter([main]));
}

#[test]
fn dependents_are_transitive_and_survive_a_cycle() {
    let db = Db::new(workspace(
        "/tmp/solrepr_deps_cycle",
        &[
            ("A.sol", "import \"./B.sol\";\ncontract A {}"),
            ("B.sol", "import \"./C.sol\";\ncontract B {}"),
            ("C.sol", "contract C {}"),
        ],
    ));
    let a = indexed(&db, Utf8PathBuf::from("/tmp/solrepr_deps_cycle/A.sol"));
    let b = indexed(&db, Utf8PathBuf::from("/tmp/solrepr_deps_cycle/B.sol"));
    let c = indexed(&db, Utf8PathBuf::from("/tmp/solrepr_deps_cycle/C.sol"));

    let map = import_map(&db);
    assert_eq!(map.dependents_of([c]), FxHashSet::from_iter([a, b]));

    // `A` imports `B` and `B` imports `A`: the seed comes back through the cycle
    // instead of the walk failing to terminate.
    let db = Db::new(workspace(
        "/tmp/solrepr_deps_cycle_two",
        &[
            ("A.sol", "import \"./B.sol\";\ncontract A {}"),
            ("B.sol", "import \"./A.sol\";\ncontract B {}"),
        ],
    ));
    let a = indexed(&db, Utf8PathBuf::from("/tmp/solrepr_deps_cycle_two/A.sol"));
    let b = indexed(&db, Utf8PathBuf::from("/tmp/solrepr_deps_cycle_two/B.sol"));
    assert_eq!(
        import_map(&db).dependents_of([a]),
        FxHashSet::from_iter([a, b])
    );
}

#[test]
fn closure_names_are_relative_to_the_entry_package() {
    let fixture = fixture_path("fixtures/workspaces/foundry/remappings/vendor");
    let db = Db::new(load_roots(fixture.clone()));
    let main = indexed(&db, fixture.join("Main.sol"));

    let names = imports_closure(&db, main)
        .iter()
        .map(|unit| unit.name.as_ref())
        .collect::<Vec<_>>();
    assert_eq!(names, ["Main.sol", "vendor/dep/Dep.sol"]);
}

#[test]
fn an_unresolved_import_keeps_its_literal() {
    let root = "/tmp/solrepr_deps_unresolved";
    let db = Db::new(workspace(
        root,
        &[("Main.sol", "import \"./Missing.sol\";\ncontract Main {}")],
    ));
    let main = indexed(&db, Utf8PathBuf::from(root).join("Main.sol"));

    let edges = imports(&db, main);
    assert!(edges.resolved.is_empty());
    assert_eq!(edges.unresolved, [SmolStr::from("./Missing.sol")]);
}

/// Resolution subscribes to the root that would own the candidate, so a path
/// that appears re-runs the import that missed it.
#[test]
fn a_file_appearing_resolves_a_previously_unresolved_import() {
    let root = "/tmp/solrepr_deps_appears";
    let mut db = Db::new(workspace(
        root,
        &[("Main.sol", "import \"./Missing.sol\";\ncontract Main {}")],
    ));
    let main = indexed(&db, Utf8PathBuf::from(root).join("Main.sol"));
    assert_eq!(imports(&db, main).unresolved.len(), 1);

    db.open_file(
        Utf8PathBuf::from(root).join("Missing.sol"),
        "contract Missing {}".to_string(),
    )
    .expect("the path is under a source root");

    let edges = imports(&db, main);
    assert!(edges.unresolved.is_empty());
    assert_eq!(edges.resolved.len(), 1);
}

/// Two directives to one file are one edge in the index; each still resolves on
/// its own, because the def map binds one directive at a time.
#[test]
fn two_directives_to_one_file_are_one_edge() {
    let root = "/tmp/solrepr_deps_two_directives";
    let db = Db::new(workspace(
        root,
        &[
            (
                "A.sol",
                "import \"./B.sol\";\nimport {B} from \"./B.sol\";\ncontract A {}",
            ),
            ("B.sol", "contract B {}"),
        ],
    ));
    let a = indexed(&db, Utf8PathBuf::from(root).join("A.sol"));
    let b = indexed(&db, Utf8PathBuf::from(root).join("B.sol"));

    assert_eq!(imports(&db, a).resolved, vec![b]);
    assert_eq!(import_map(&db).of(b), &[a]);
}

/// The reverse index covers workspace roots only, but the forward direction
/// does not: a dependency can still be walked into, which is what solc needs.
#[test]
fn a_dependency_imports_the_workspace_but_is_not_in_the_index() {
    let root = Utf8PathBuf::from("/tmp/solrepr_deps_dependency");
    let package = PackageId(NormalizedPath::new(root.clone()));
    let db = Db::new(WorkspaceWithFiles {
        ignored_roots: Vec::new(),
        packages: vec![Package {
            root: NormalizedPath::new(root.clone()),
            remappings: Vec::new(),
        }],
        roots: vec![
            RootWithFiles {
                path: NormalizedPath::new(root.join("src")),
                package_id: package.clone(),
                is_dependency: false,
                files: vec![LoadedFile {
                    path: NormalizedPath::new(root.join("src/A.sol")),
                    text: Some(Rope::from_str("contract A {}")),
                }],
            },
            RootWithFiles {
                path: NormalizedPath::new(root.join("lib/dep")),
                package_id: package,
                is_dependency: true,
                files: vec![LoadedFile {
                    path: NormalizedPath::new(root.join("lib/dep/D.sol")),
                    text: Some(Rope::from_str("import \"src/A.sol\";\ncontract D {}")),
                }],
            },
        ],
    });
    let a = indexed(&db, root.join("src/A.sol"));
    let d = indexed(&db, root.join("lib/dep/D.sol"));

    assert_eq!(imports(&db, d).resolved, vec![a]);
    assert!(
        import_map(&db).of(a).is_empty(),
        "a dependency root is outside the reverse index"
    );
}
