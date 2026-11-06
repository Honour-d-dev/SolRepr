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
use crate::salsa::db::{Db, File};
use crate::salsa::{HirDb, RootDb};
use crate::workspace::LoadedFile;

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

fn fixture() -> (Db, File, Recorder) {
    let recorder = Recorder::default();
    let sink = recorder.clone();
    let workspace = test_workspace(SOURCE);
    let path = workspace.source_roots[0].files[0].path.clone();
    let db = Db::with_events(workspace, move |event| {
        if let salsa::EventKind::WillExecute { database_key } = event.kind {
            sink.seen.lock().unwrap().push(format!("{database_key:?}"));
        }
    });
    let file = db.file(&path).unwrap();
    (db, file, recorder)
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
    let root = workspace.source_roots[0].root.clone();
    let other_path = root.join("src/Other.sol");
    workspace.source_roots[0].files.push(LoadedFile {
        path: other_path.clone(),
        text: Rope::from_str(OTHER),
    });
    let path = workspace.source_roots[0].files[0].path.clone();
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
