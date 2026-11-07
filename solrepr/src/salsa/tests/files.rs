use crate::salsa::Db;
use crate::utilities::fixture_path;
use crate::workspace::loader::load_root;
use camino::Utf8PathBuf;

fn standalone_db() -> Db {
    let root = fixture_path("fixtures/workspaces/standalone/basic");
    Db::new(load_root(root))
}

/// A stray document still attaches when a source root encloses it —
/// here the frameworkless container root covers the whole tree.
#[test]
fn stray_file_under_a_source_root_attaches_to_that_root() {
    let root = fixture_path("fixtures/workspaces/foundry/container");
    let mut db = Db::new(load_root(root.clone()));
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
    let mut db = Db::new(load_root(root));
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
    let db = Db::new(load_root(fixture.clone()));
    let main = db
        .file(&fixture.join("Main.sol"))
        .expect("the workspace indexes Main.sol");
    let dep = db
        .resolve_to_file(main, "dep/Dep.sol")
        .expect("dep/ resolves through remappings.txt");
    assert!(dep.path(&db).ends_with("vendor/dep/Dep.sol"));
}

/// A remapping target outside the workspace root is indexed as a
/// synthesized dependency source root and resolves normally.
#[test]
fn external_remapping_target_resolves_into_a_synthesized_dependency() {
    let fixture = fixture_path("fixtures/workspaces/foundry/remappings/external/proj");
    let db = Db::new(load_root(fixture.clone()));
    let main = db
        .file(&fixture.join("Main.sol"))
        .expect("the workspace indexes Main.sol");
    let ext = db
        .resolve_to_file(main, "ext/Ext.sol")
        .expect("ext/ resolves into the synthesized dependency");
    assert!(ext.path(&db).ends_with("ext_lib/Ext.sol"));
}

/// A manifestless vendored library is indexed as a named dependency —
/// the `lib` edge is trusted — and the auto-detected library remapping
/// resolves imports into it.
#[test]
fn manifestless_library_resolves_through_the_synthesized_remapping() {
    let fixture = fixture_path("fixtures/workspaces/foundry/nested");
    let db = Db::new(load_root(fixture.clone()));
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
    let db = Db::new(load_root(fixture.clone()));
    let main = db
        .file(&fixture.join("src/Root.sol"))
        .expect("the workspace indexes src/Root.sol");
    let dep = db
        .resolve_to_file(main, "fs/Lib.sol")
        .expect("fs/ resolves into the library's src dir");
    assert!(dep.path(&db).ends_with("dependencies/fs/src/Lib.sol"));
}

/// A remapping dependency's own remappings resolve transitively —
/// `ext_a` imports through `b/` into `ext_b`.
#[test]
fn transitive_remapping_dependencies_resolve_imports() {
    let fixture = fixture_path("fixtures/workspaces/foundry/remappings/external");
    let db = Db::new(load_root(fixture.clone()));
    let main = db
        .file(&fixture.join("proj/Main.sol"))
        .expect("the workspace indexes proj/Main.sol");
    let a = db
        .resolve_to_file(main, "a/A.sol")
        .expect("a/ resolves into ext_a");
    assert!(a.path(&db).ends_with("ext_a/A.sol"));
    let b = db
        .resolve_to_file(a, "b/B.sol")
        .expect("ext_a's own b/ remapping resolves into ext_b");
    assert!(b.path(&db).ends_with("ext_b/B.sol"));
}

/// A file inside a library the package graph deduplicated away is never
/// indexed — `is_ignored_path` marks it, and opening it produces no
/// `File` and no source root, so it cannot sync text or be analyzed.
#[test]
fn deduplicated_library_files_are_never_indexed() {
    let root = fixture_path("fixtures/workspaces/foundry/nested");
    let mut db = Db::new(load_root(root));
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
    let mut db = Db::new(load_root(root));

    assert!(db.is_ignored_path(&path));
    assert!(
        db.open_file(path.clone(), "contract T {}".to_string())
            .is_none(),
        "a dependency dev file gets no File and no source root"
    );
    assert!(db.file(&path).is_none());
}
