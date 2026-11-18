use crate::salsa::Db;
use crate::utilities::fixture_path;
use crate::workspace::loader::load_roots;
use crate::workspace::{
    LoadedFile, Package, PackageId, RootWithFiles, WorkspaceWithFiles,
};
use camino::Utf8PathBuf;
use ropey::Rope;

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
    let source_root = db.file_source_root(file).expect("file has a source root");
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
    let path = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/workspaces/AdHocScratch.sol");

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
    let path = fixture_path("fixtures/workspaces/foundry/nested/dependencies/dep-a/lib/duplicate/src/Duplicate.sol");

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
        let discovered = crate::workspace::Workspace::<()>::from(&crate::workspace::discover_workspaces(std::slice::from_ref(&root)));
        assert_eq!(db.workspace(), discovered, "{fixture}");
    }
}

/// A non-relative import resolves against the package root as well as the
/// remapping table — Foundry's base path, which is how `import "src/Foo.sol"`
/// from a test file finds its target.
#[test]
fn a_project_rooted_import_resolves_against_the_package_root() {
    let root = Utf8PathBuf::from("/tmp/project_rooted_import");
    let package = PackageId(root.clone());
    let workspace = WorkspaceWithFiles {
        ignored_roots: Vec::new(),
        packages: vec![Package {
            root: root.clone(),
            remappings: Vec::new(),
        }],
        roots: vec![
            RootWithFiles {
                path: root.join("src"),
                package_id: package.clone(),
                is_dependency: false,
                files: vec![LoadedFile {
                    path: root.join("src/Foo.sol"),
                    text: Some(Rope::from_str("contract Foo {}")),
                }],
            },
            RootWithFiles {
                path: root.join("test"),
                package_id: package,
                is_dependency: false,
                files: vec![LoadedFile {
                    path: root.join("test/Foo.t.sol"),
                    text: Some(Rope::from_str("import \"src/Foo.sol\";")),
                }],
            },
        ],
    };
    let db = Db::new(workspace);
    let from = db.file(&root.join("test/Foo.t.sol")).expect("test file is indexed");
    let resolved = db.resolve_to_file(from, "src/Foo.sol");
    assert_eq!(
        resolved.map(|file| file.path(&db).clone()),
        Some(root.join("src/Foo.sol"))
    );
}

