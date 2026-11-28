//! Cache-invalidation regression tests driven by salsa's event callback.
//!
//! `WillExecute` fires whenever a query re-derives its value — including when
//! it only re-derives to prove the value is unchanged. So the interesting
//! assertion is never "nothing ran"; it is which queries stay *cut off*
//! downstream. An edit anywhere in a file re-lowers that file's item data and
//! bodies (they read the AST), but the `*_data` and `body` guards above them
//! compare content alone, so the def map and every untouched body's inference
//! keep their memoized values.

use std::sync::{Arc, Mutex};

use camino::Utf8PathBuf;
use lsp_types::{Position, Range, TextDocumentContentChangeEvent};
use ropey::Rope;

use crate::capabilities::diagnostics::lsp_diagnostics;
use crate::hir::tests::helper::test_workspace;
use crate::ir::def_map::DefId;
use crate::paths::NormalizedPath;
use crate::salsa::db::{Db, File};
use crate::salsa::root_db::{import_map, imports};
use crate::salsa::{HirDb, RootDb};
use crate::workspace::diff::diff;
use crate::workspace::{LoadedFile, Package, PackageId, Remapping, RootWithFiles, WorkspaceWithFiles};

const SOURCE: &str = r#"
contract C {
    uint x;

    function a() public {
        uint y = 1;
    }

    function b() public {
        uint z = 2;
    }
}
"#;

/// Records the debug key of every `WillExecute` event, e.g.
/// `inference(Id(1000))`.
#[derive(Clone, Default)]
struct Recorder {
    seen: Arc<Mutex<Vec<String>>>,
}

impl Recorder {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut self.seen.lock().unwrap())
    }
}

/// The number of re-derived values of `query`.
fn ran(events: &[String], query: &str) -> usize {
    let prefix = format!("{query}(");
    events.iter().filter(|key| key.starts_with(&prefix)).count()
}

#[salsa::tracked]
fn root_count(db: &dyn RootDb) -> usize {
    db.source_roots().len()
}

fn fixture() -> (Db, File, Recorder) {
    let recorder = Recorder::default();
    let sink = recorder.clone();
    let workspace = test_workspace(SOURCE);
    let path = workspace.roots[0].files[0].path.clone();
    let db = Db::with_events(workspace, move |event| {
        if let salsa::EventKind::WillExecute { database_key } = event.kind {
            sink.seen.lock().unwrap().push(format!("{database_key:?}"));
        }
    });
    let file = db.file(&path).unwrap();
    (db, file, recorder)
}

fn recording(workspace: WorkspaceWithFiles<LoadedFile>) -> (Db, Recorder) {
    let recorder = Recorder::default();
    let sink = recorder.clone();
    let db = Db::with_events(workspace, move |event| {
        if let salsa::EventKind::WillExecute { database_key } = event.kind {
            sink.seen.lock().unwrap().push(format!("{database_key:?}"));
        }
    });
    (db, recorder)
}

fn loaded(path: &str, text: &str) -> LoadedFile {
    LoadedFile {
        path: NormalizedPath::new(path),
        text: Some(Rope::from_str(text)),
    }
}

fn root(path: &str, files: Vec<LoadedFile>) -> RootWithFiles<LoadedFile> {
    RootWithFiles {
        path: NormalizedPath::new(path),
        package_id: PackageId(NormalizedPath::new(path)),
        is_dependency: false,
        files,
    }
}

fn workspace_of(roots: Vec<RootWithFiles<LoadedFile>>) -> WorkspaceWithFiles<LoadedFile> {
    WorkspaceWithFiles {
        ignored_roots: Vec::new(),
        packages: roots
            .iter()
            .map(|root| Package {
                root: root.path.clone(),
                remappings: Vec::new(),
            })
            .collect(),
        roots,
    }
}

fn function_named(db: &Db, file: File, name: &str) -> DefId {
    db.file_def_map(file)
        .defs
        .keys()
        .find(|def| match def {
            DefId::Function(id) => db.function_data(*id).name.as_str() == name,
            _ => false,
        })
        .copied()
        .unwrap_or_else(|| panic!("fixture declares no function `{name}`"))
}

/// Replaces `len` bytes at `byte` with `text`.
fn edit(db: &mut Db, file: File, byte: usize, len: usize, text: &str) {
    let rope = db.rope(file);
    let idx = rope.byte_to_char(byte);
    let line = rope.char_to_line(idx);
    let character = idx - rope.line_to_char(line);
    let path: Utf8PathBuf = file.path(db).to_path_buf();
    db.apply_changes(
        path,
        vec![TextDocumentContentChangeEvent {
            range: Some(Range {
                start: Position {
                    line: line as u32,
                    character: character as u32,
                },
                end: Position {
                    line: line as u32,
                    character: (character + len) as u32,
                },
            }),
            range_length: Some(len as u32),
            text: text.to_owned(),
        }],
    );
}

/// An edit outside every body re-lowers the file's item data and bodies but
/// invalidates no semantic result.
#[test]
fn out_of_body_edit_runs_no_inference() {
    let (mut db, file, recorder) = fixture();
    let a = function_named(&db, file, "a");
    let b = function_named(&db, file, "b");
    let _ = db.inference(a);
    let _ = db.inference(b);
    let _ = db.file_def_map(file);
    recorder.take();

    // A blank line at the top shifts every declaration's offset.
    edit(&mut db, file, 0, 0, "\n");

    let _ = db.inference(a);
    let _ = db.inference(b);
    let _ = db.file_def_map(file);

    let events = recorder.take();
    assert_eq!(ran(&events, "inference"), 0, "{events:#?}");
    assert_eq!(ran(&events, "root_def_map_analysis"), 0, "{events:#?}");
    // The item data and bodies do re-lower — that is the comparison step, and
    // their unchanged values are what cut the queries above off.
    assert_eq!(ran(&events, "build_function"), 2, "{events:#?}");
    assert_eq!(ran(&events, "function_data"), 2, "{events:#?}");
    assert_eq!(ran(&events, "build_body"), 2, "{events:#?}");
    assert_eq!(ran(&events, "body"), 2, "{events:#?}");
}

/// Opening an unmodified document does not rewrite its text or re-parse it.
#[test]
fn opening_an_unchanged_document_keeps_parse_cached() {
    let (mut db, file, recorder) = fixture();
    let _ = db.item_tree(file);
    recorder.take();

    let path = file.path(&db).to_path_buf();
    db.open_file(path, SOURCE.to_string()).expect("the document is indexed");

    let _ = db.item_tree(file);
    let events = recorder.take();
    assert_eq!(ran(&events, "parse"), 0, "{events:#?}");
    assert_eq!(ran(&events, "item_tree"), 0, "{events:#?}");
}

/// An edit inside one body invalidates exactly that body's inference.
#[test]
fn in_body_edit_runs_only_that_inference() {
    let (mut db, file, recorder) = fixture();
    let a = function_named(&db, file, "a");
    let b = function_named(&db, file, "b");
    let _ = db.inference(a);
    let _ = db.inference(b);
    recorder.take();

    // `uint y = 1;` -> `uint y = 3;`
    let offset = SOURCE.find("uint y = 1;").unwrap() + "uint y = ".len();
    edit(&mut db, file, offset, 1, "3");

    let _ = db.inference(a);
    let _ = db.inference(b);

    let events = recorder.take();
    assert_eq!(ran(&events, "inference"), 1, "{events:#?}");
    // Both bodies re-lower, since both depend on the parse — but `b`'s lowered
    // value is identical, so even its `body` guard is cut off.
    assert_eq!(ran(&events, "build_body"), 2, "{events:#?}");
    assert_eq!(ran(&events, "body"), 1, "{events:#?}");
    // The edit keeps every span, so `a`'s item data never even re-derives.
    assert_eq!(ran(&events, "function_data"), 0, "{events:#?}");
}

/// A second file in the same source root, for the cross-file assertions.
const OTHER: &str = r#"
contract D {
    function c() public {
        uint w = 1;
    }
}
"#;

/// The LSP conversion is memoized, so an edit in another file does not
/// re-convert a file whose own diagnostics are unchanged.
#[test]
fn lsp_diagnostics_survives_an_edit_in_another_file() {
    let recorder = Recorder::default();
    let sink = recorder.clone();
    let mut workspace = test_workspace(SOURCE);
    let root = workspace.roots[0].path.clone();
    let other_path = NormalizedPath::new(root.join("src/Other.sol"));
    workspace.roots[0].files.push(LoadedFile {
        path: other_path.clone(),
        text: Some(Rope::from_str(OTHER)),
    });
    let path = workspace.roots[0].files[0].path.clone();
    let mut db = Db::with_events(workspace, move |event| {
        if let salsa::EventKind::WillExecute { database_key } = event.kind {
            sink.seen.lock().unwrap().push(format!("{database_key:?}"));
        }
    });
    let file = db.file(&path).unwrap();
    let other = db.file(&other_path).unwrap();

    let _ = lsp_diagnostics(&db, file);
    recorder.take();

    // Same-length edit inside the other file's body.
    let needle = "uint w = 1;";
    let offset = OTHER.find(needle).unwrap();
    edit(&mut db, other, offset, needle.len(), "uint w = 3;");

    let _ = lsp_diagnostics(&db, file);

    let events = recorder.take();
    // The other file re-parses and re-lowers, but its item tree compares
    // equal — so the root def map and this file's conversion are both
    // validated without executing.
    assert_eq!(ran(&events, "item_tree"), 1, "{events:#?}");
    assert_eq!(ran(&events, "root_def_map_analysis"), 0, "{events:#?}");
    assert_eq!(ran(&events, "lsp_diagnostics"), 0, "{events:#?}");
}

/// A file appearing in one root re-collects that root's inversion only: the
/// other roots' maps are validated without executing.
#[test]
fn a_new_file_re_collects_one_roots_inversion() {
    let workspace = workspace_of(vec![
        root(
            "/w/src",
            vec![
                loaded("/w/src/A.sol", "import \"./B.sol\";\ncontract A {}\n"),
                loaded("/w/src/B.sol", "contract B {}\n"),
            ],
        ),
        root(
            "/w/test",
            vec![
                loaded("/w/test/T.sol", "import \"./U.sol\";\ncontract T {}\n"),
                loaded("/w/test/U.sol", "contract U {}\n"),
            ],
        ),
    ]);
    let (mut db, recorder) = recording(workspace);
    let src = db.source_roots()[0];
    let test = db.source_roots()[1];
    let _ = import_map(&db, src);
    let _ = import_map(&db, test);
    recorder.take();

    db.open_file(Utf8PathBuf::from("/w/src/New.sol"), "contract New {}".to_string())
        .expect("indexed under src");

    // The untouched root: neither its map nor its files' imports re-run.
    let _ = import_map(&db, test);
    let events = recorder.take();
    assert_eq!(ran(&events, "import_map_of"), 0, "{events:#?}");
    assert_eq!(ran(&events, "imports"), 0, "{events:#?}");

    // The root that gained the file: its map re-collects, and only `A`
    // re-resolves — `New.sol` is computed for the first time.
    let _ = import_map(&db, src);
    let events = recorder.take();
    assert_eq!(ran(&events, "import_map_of"), 1, "{events:#?}");
    assert_eq!(ran(&events, "imports"), 2, "{events:#?}");
}

/// An import that resolved to nothing is re-resolved when the file appears.
#[test]
fn an_unresolved_import_resolves_when_the_file_appears() {
    let workspace = workspace_of(vec![root(
        "/w/src",
        vec![loaded("/w/src/A.sol", "import \"./Missing.sol\";\ncontract A {}\n")],
    )]);
    let (mut db, recorder) = recording(workspace);
    let a = db.file(&Utf8PathBuf::from("/w/src/A.sol")).unwrap();
    assert!(imports(&db, a).resolved.is_empty());
    recorder.take();

    db.open_file(Utf8PathBuf::from("/w/src/Missing.sol"), "contract Missing {}".to_string())
        .expect("indexed under src");

    assert_eq!(imports(&db, a).resolved.len(), 1, "the import resolves once the file exists");
}

/// A reload that moves only package facts does not rewrite the root set.
#[test]
fn manifest_only_reload_does_not_invalidate_root_set() {
    let workspace = workspace_of(vec![
        root("/w/src", vec![loaded("/w/src/A.sol", "contract A {}\n")]),
        root("/w/test", vec![loaded("/w/test/T.sol", "contract T {}\n")]),
    ]);
    let (mut db, recorder) = recording(workspace);
    let test = db.source_roots()[1];
    let _ = root_count(&db);
    let _ = import_map(&db, test);
    recorder.take();

    let mut reloaded = workspace_of(vec![
        root("/w/src", vec![loaded("/w/src/A.sol", "contract A {}\n")]),
        root("/w/test", vec![loaded("/w/test/T.sol", "contract T {}\n")]),
    ]);
    reloaded
        .packages
        .iter_mut()
        .find(|package| package.root.as_path() == "/w/src")
        .unwrap()
        .remappings
        .push(Remapping {
            context: None,
            prefix: "dep/".to_string(),
            target: NormalizedPath::new("/w/lib/dep"),
        });
    let (_, prepared) = diff(&db.workspace(), reloaded);
    db.reload_workspace(prepared);

    let _ = root_count(&db);
    let _ = import_map(&db, test);

    let events = recorder.take();
    assert_eq!(ran(&events, "root_count"), 0, "{events:#?}");
    assert_eq!(ran(&events, "import_map_of"), 0, "{events:#?}");
}

/// A root appearing elsewhere is not an edge of an import that already resolved.
#[test]
fn adding_a_new_root_does_not_re_run_resolved_imports() {
    let workspace = workspace_of(vec![root(
        "/w/src",
        vec![
            loaded("/w/src/A.sol", "import \"./B.sol\";\ncontract A {}\n"),
            loaded("/w/src/B.sol", "contract B {}\n"),
        ],
    )]);
    let (mut db, recorder) = recording(workspace);
    let src = db.source_roots()[0];
    let a = db.file(&Utf8PathBuf::from("/w/src/A.sol")).unwrap();
    let _ = imports(&db, a);
    let _ = import_map(&db, src);
    recorder.take();

    let reloaded = workspace_of(vec![
        root(
            "/w/src",
            vec![
                loaded("/w/src/A.sol", "import \"./B.sol\";\ncontract A {}\n"),
                loaded("/w/src/B.sol", "contract B {}\n"),
            ],
        ),
        root("/w/lib/dep", vec![loaded("/w/lib/dep/Dep.sol", "contract Dep {}\n")]),
    ]);
    let (_, prepared) = diff(&db.workspace(), reloaded);
    db.reload_workspace(prepared);

    let _ = imports(&db, a);
    let _ = import_map(&db, src);

    let events = recorder.take();
    assert_eq!(ran(&events, "imports"), 0, "{events:#?}");
    assert_eq!(ran(&events, "import_map_of"), 0, "{events:#?}");
}

/// Re-parenting a file to a narrower root moves the handle, not its text: the
/// parse and the item tree keep their memos.
#[test]
fn re_parenting_a_file_does_not_re_parse_it() {
    let workspace = workspace_of(vec![root("/w", vec![loaded("/w/lib/dep/Dep.sol", "contract Dep {}\n")])]);
    let (mut db, recorder) = recording(workspace);
    let dep = db.file(&Utf8PathBuf::from("/w/lib/dep/Dep.sol")).unwrap();
    let _ = db.item_tree(dep);
    recorder.take();

    let narrowed = workspace_of(vec![
        root("/w", Vec::new()),
        root("/w/lib/dep", vec![loaded("/w/lib/dep/Dep.sol", "contract Dep {}\n")]),
    ]);
    let (_, prepared) = diff(&db.workspace(), narrowed);
    db.reload_workspace(prepared);
    assert_eq!(
        db.file_source_root(dep).root(&db).as_path(),
        Utf8PathBuf::from("/w/lib/dep").as_path(),
        "the handle moved to the narrower root"
    );

    let _ = db.item_tree(dep);

    let events = recorder.take();
    assert_eq!(ran(&events, "parse"), 0, "{events:#?}");
    assert_eq!(ran(&events, "item_tree"), 0, "{events:#?}");
}
