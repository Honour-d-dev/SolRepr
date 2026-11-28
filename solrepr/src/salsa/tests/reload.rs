use std::fs;

use camino::Utf8PathBuf;

use ropey::Rope;
use rustc_hash::FxHashSet;

use crate::hir::diagnostics::SemanticDiagnostic;
use crate::paths::NormalizedPath;
use crate::salsa::{Db, RootDb};
use crate::workspace::diff::{WorkspaceDiff, diff};
use crate::workspace::watch::ChangeEvent;
use crate::workspace::{LoadedFile, Package, PackageId, RootWithFiles, WorkspaceWithFiles};

/// A throwaway tree on disk: reload reads newly discovered paths from it.
struct TempTree {
    root: NormalizedPath,
}

impl TempTree {
    fn new(name: &str) -> Self {
        let root = Utf8PathBuf::from_path_buf(std::env::temp_dir().join(format!("solrepr_reload_{name}_{}", std::process::id())))
            .expect("temp dir is UTF-8");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).expect("create src");
        // Discovery canonicalizes the root, and on Windows that returns the
        // on-disk casing: `%TEMP%` may spell it differently, and path identity
        // is case-sensitive.
        let root = NormalizedPath::new(root);
        Self { root }
    }

    fn write(&self, relative: &str, text: &str) {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create dir");
        }
        fs::write(&path, text).expect("write file");
    }

    fn discover(&self) -> WorkspaceWithFiles<NormalizedPath> {
        crate::workspace::discover_workspaces(std::slice::from_ref(&self.root))
    }

    fn path(&self, relative: &str) -> NormalizedPath {
        NormalizedPath::new(self.root.join(relative))
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn with_dependency(tree: &TempTree) {
    tree.write("lib/dep/foundry.toml", "[profile.default]\n");
    tree.write("lib/dep/Dep.sol", "contract Dep {}\n");
}

/// What the reload worker would hand the server for this discovery.
fn reload(db: &mut Db, workspace: WorkspaceWithFiles<NormalizedPath>) -> WorkspaceDiff<LoadedFile> {
    let (_, prepared) = diff(&db.workspace(), workspace);
    let prepared: WorkspaceDiff<LoadedFile> = prepared.into();
    db.reload_workspace(prepared.clone());
    prepared
}

fn unresolved(db: &Db, file: crate::salsa::File) -> bool {
    let root = db.file_source_root(file);
    db.root_def_map_analysis(root)
        .diagnostics
        .iter()
        .any(|diagnostic| matches!(diagnostic, SemanticDiagnostic::UnresolvedImport { .. }))
}

/// The import is unresolvable until discovery finds the dependency root, and
/// the reload has to make the already-memoized def map see it.
#[test]
fn adding_a_dependency_root_clears_an_unresolved_import() {
    let tree = TempTree::new("add_dependency");
    tree.write("foundry.toml", "[profile.default]\n");
    tree.write("src/A.sol", "import \"../lib/dep/Dep.sol\";\ncontract A {}\n");

    let mut db = Db::new(tree.discover());
    let a = db.file(tree.path("src/A.sol")).expect("A is indexed");
    assert!(unresolved(&db, a), "the dependency does not exist yet");

    with_dependency(&tree);
    let diff = reload(&mut db, tree.discover());

    assert!(!diff.is_empty());
    assert_eq!(
        diff.roots_added.iter().map(|root| root.path.clone()).collect::<Vec<_>>(),
        vec![tree.path("lib/dep")]
    );
    assert_eq!(
        db.workspace(),
        crate::workspace::Workspace::<()>::from(&tree.discover()),
        "the reload leaves the live root set stale"
    );
    assert!(!unresolved(&db, a), "the import resolves after the reload");
}

/// Removing the root has to make the importer report the import again, and the
/// retired file must stop being import-resolvable.
#[test]
fn removing_a_root_reports_the_import_as_unresolved() {
    let tree = TempTree::new("remove_root");
    tree.write("foundry.toml", "[profile.default]\n");
    tree.write("src/A.sol", "import \"../lib/dep/Dep.sol\";\ncontract A {}\n");
    with_dependency(&tree);

    let mut db = Db::new(tree.discover());
    let a = db.file(tree.path("src/A.sol")).expect("A is indexed");
    let dep = db.file(tree.path("lib/dep/Dep.sol")).expect("Dep is indexed");
    assert!(!unresolved(&db, a));

    fs::remove_dir_all(tree.root.join("lib/dep")).expect("remove the dependency");
    let diff = reload(&mut db, tree.discover());

    assert_eq!(diff.removed, FxHashSet::from_iter([tree.path("lib/dep")]));
    assert!(unresolved(&db, a));
    assert!(db.file(tree.path("lib/dep/Dep.sol")).is_none());

    // The handle outlives the index, so a queued diagnostics request holding it
    // still resolves its root.
    assert_eq!(db.file_source_root(dep).root(&db).as_path(), tree.path("lib/dep").as_path());
}

#[test]
fn an_unchanged_discovery_is_a_no_op() {
    let tree = TempTree::new("no_op");
    tree.write("foundry.toml", "[profile.default]\n");
    tree.write("src/A.sol", "contract A {}\n");
    with_dependency(&tree);

    let mut db = Db::new(tree.discover());
    let before = db.workspace();

    let diff = reload(&mut db, tree.discover());

    assert!(diff.is_empty());
    assert_eq!(db.workspace(), before);
}

/// A root arriving must not touch the documents that were already open: the
/// client owns their text, and their handles stay put.
#[test]
fn open_document_text_survives_a_reload() {
    let tree = TempTree::new("open_document");
    tree.write("foundry.toml", "[profile.default]\n");
    tree.write("src/A.sol", "contract A {}\n");

    let mut db = Db::new(tree.discover());
    let path = tree.path("src/A.sol");
    let a = db
        .open_file(path.clone(), "contract Edited {}".to_string())
        .expect("the document is indexed");

    with_dependency(&tree);
    reload(&mut db, tree.discover());

    assert_eq!(db.file(&path), Some(a), "the handle survives");
    assert_eq!(db.rope(a).to_string(), "contract Edited {}");
}

fn loaded(root: &str, package: &str, files: &[(&str, &str)]) -> WorkspaceWithFiles<LoadedFile> {
    WorkspaceWithFiles {
        packages: vec![Package {
            root: NormalizedPath::new(package),
            remappings: Vec::new(),
        }],
        roots: vec![RootWithFiles {
            path: NormalizedPath::new(root),
            package_id: PackageId(NormalizedPath::new(package)),
            files: files
                .iter()
                .map(|(path, text)| LoadedFile {
                    path: NormalizedPath::new(*path),
                    text: Some(Rope::from_str(text)),
                })
                .collect(),
            is_dependency: false,
        }],
        ignored_roots: Vec::new(),
    }
}

fn discovered(root: &str, package: &str, files: &[&str]) -> WorkspaceWithFiles<NormalizedPath> {
    WorkspaceWithFiles {
        packages: vec![Package {
            root: NormalizedPath::new(package),
            remappings: Vec::new(),
        }],
        roots: vec![RootWithFiles {
            path: NormalizedPath::new(root),
            package_id: PackageId(NormalizedPath::new(package)),
            files: files.iter().map(NormalizedPath::new).collect(),
            is_dependency: false,
        }],
        ignored_roots: Vec::new(),
    }
}

/// A file can move from a root that *survives* to a more specific root that
/// appears. The parent's file list is a snapshot from before the nested root
/// existed, and a reload never recomputes a surviving root, so the detach in
/// apply is what keeps one path out of two roots' lists.
#[test]
fn a_nested_root_takes_its_files_from_the_surviving_parent() {
    let mut db = Db::new(loaded(
        "/proj",
        "/proj",
        &[("/proj/A.sol", "contract A {}"), ("/proj/lib/dep/Dep.sol", "contract Dep {}")],
    ));
    let dep = db.file(Utf8PathBuf::from("/proj/lib/dep/Dep.sol")).expect("Dep is indexed");
    let parent = db.file_source_root(dep);
    assert!(parent.files(&db).contains(&dep), "the parent owns it to begin with");

    // Discovery now claims the file for a root of its own. The parent is still
    // discovered, with the same path, package and flag, so it survives.
    let mut workspace = discovered("/proj", "/proj", &["/proj/A.sol"]);
    workspace.packages.push(Package {
        root: NormalizedPath::new("/proj/lib/dep"),
        remappings: Vec::new(),
    });
    workspace.roots.push(RootWithFiles {
        path: NormalizedPath::new("/proj/lib/dep"),
        package_id: PackageId(NormalizedPath::new("/proj/lib/dep")),
        files: vec![NormalizedPath::new("/proj/lib/dep/Dep.sol")],
        is_dependency: true,
    });

    let diff = reload(&mut db, workspace);

    assert!(diff.removed.is_empty(), "the parent survives, so only the nested root is new");
    let root = db.file_source_root(dep);
    assert_ne!(root, parent, "the handle is re-pointed at the nested root");
    assert!(
        !parent.files(&db).contains(&dep),
        "the parent no longer lists a file the nested root owns"
    );
}

/// A root whose package moved is replaced, but its files are not: the path is
/// still indexed, so the handle and the client's text survive.
#[test]
fn a_root_replaced_in_place_keeps_its_file_handles_and_text() {
    let path = Utf8PathBuf::from("/replaced/root/A.sol");
    let mut db = Db::new(loaded("/replaced/root", "/replaced", &[("/replaced/root/A.sol", "contract A {}")]));
    let a = db.file(&path).expect("A is indexed");
    let before = db.file_source_root(a);
    db.open_file(path.clone(), "contract Edited {}".to_string())
        .expect("the document is indexed");

    let diff = reload(&mut db, discovered("/replaced/root", "/replaced/other", &["/replaced/root/A.sol"]));

    assert!(!diff.is_empty());
    assert_eq!(
        diff.roots_added.iter().map(|root| root.path.clone()).collect::<Vec<_>>(),
        vec![NormalizedPath::new("/replaced/root")],
        "the root is replaced because its package moved"
    );
    assert_eq!(db.file(&path), Some(a), "the handle survives the replacement");
    assert_eq!(db.rope(a).to_string(), "contract Edited {}");

    // The handle must be in the *new* root's list: a reload that keeps the
    // path mapped but drops it from the root would pass the assertions above
    // while leaving the file out of the compilation unit.
    let root = db.file_source_root(a);
    assert_ne!(
        root, before,
        "the handle is re-pointed at the replacement root, not left on the old one"
    );
    assert_eq!(root.root(&db), &NormalizedPath::new("/replaced/root"));
    assert!(root.files(&db).contains(&a), "the handle is in its root's file list");
    assert_eq!(
        db.workspace(),
        crate::workspace::Workspace::<()>::from(&discovered("/replaced/root", "/replaced/other", &["/replaced/root/A.sol"],)),
        "the replacement root is not carried alongside the old one"
    );
}

/// A file appearing inside a known root is a single-path operation: no
/// discovery walk, and the importer that failed before now resolves.
#[test]
fn a_file_created_on_disk_clears_an_unresolved_import() {
    let tree = TempTree::new("create_file");
    tree.write("foundry.toml", "[profile.default]\n");
    tree.write("src/A.sol", "import \"./B.sol\";\ncontract A {}\n");

    let mut db = Db::new(tree.discover());
    let a = db.file(tree.path("src/A.sol")).expect("A is indexed");
    assert!(unresolved(&db, a), "B does not exist yet");

    tree.write("src/B.sol", "contract B {}\n");
    let ChangeEvent::Created(b) = db.apply_file_change(&tree.path("src/B.sol")) else {
        panic!("the new file is indexed");
    };

    assert_eq!(db.file(tree.path("src/B.sol")), Some(b));
    assert!(!unresolved(&db, a), "the import resolves");
    assert_eq!(
        db.apply_file_change(&tree.path("src/B.sol")),
        ChangeEvent::Skipped,
        "unchanged text is a no-op"
    );
    tree.write("src/B.sol", "contract B { uint256 value; }\n");
    assert_eq!(
        db.apply_file_change(&tree.path("src/B.sol")),
        ChangeEvent::Updated(b),
        "changed text updates the same handle"
    );
}

/// Deleting one retires the path without touching the root set, and the
/// importer reports it again.
#[test]
fn a_file_deleted_on_disk_reports_the_import_as_unresolved() {
    let tree = TempTree::new("delete_file");
    tree.write("foundry.toml", "[profile.default]\n");
    tree.write("src/A.sol", "import \"./B.sol\";\ncontract A {}\n");
    tree.write("src/B.sol", "contract B {}\n");

    let mut db = Db::new(tree.discover());
    let a = db.file(tree.path("src/A.sol")).expect("A is indexed");
    let b = db.file(tree.path("src/B.sol")).expect("B is indexed");
    assert!(!unresolved(&db, a));

    fs::remove_file(tree.path("src/B.sol")).expect("remove B");
    assert_eq!(
        db.apply_file_change(&tree.path("src/B.sol")),
        ChangeEvent::Removed(b),
        "the vanished path is retired"
    );
    assert!(db.file(tree.path("src/B.sol")).is_none());
    assert_eq!(
        db.file_source_root(b).root(&db).as_path(),
        tree.path("src").as_path(),
        "the root survives on the handle"
    );
    assert!(unresolved(&db, a), "the import is reported again");
}

/// `source_roots` reads the root-set input, so a reload that adds a root
/// re-runs a query that read it.
#[salsa::tracked]
fn root_paths(db: &dyn RootDb) -> Vec<NormalizedPath> {
    db.source_roots().iter().map(|root| root.root(db).clone()).collect()
}

#[test]
fn source_roots_invalidates_when_a_reload_adds_a_root() {
    let tree = TempTree::new("source_roots_added");
    tree.write("foundry.toml", "[profile.default]\n");
    tree.write("src/A.sol", "contract A {}\n");
    let mut db = Db::new(tree.discover());
    assert_eq!(root_paths(&db).len(), 1);

    with_dependency(&tree);
    reload(&mut db, tree.discover());
    assert_eq!(root_paths(&db).len(), 2, "{:?}", root_paths(&db));
}
