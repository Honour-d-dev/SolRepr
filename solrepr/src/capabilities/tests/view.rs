use camino::Utf8Path;
use lsp_server::{Request, RequestId};
use lsp_types::request::Request as _;
use serde_json::json;

use crate::capabilities::view_hir::{ViewHirRequest, view_hir};
use crate::hir::tests::helper::make_test_db;
use crate::salsa::Db;
use crate::utilities::to_url;

fn view(db: &Db, path: &Utf8Path, start: (u32, u32), end: (u32, u32)) -> String {
    let response = view_hir(
        db,
        Request {
            id: RequestId::from(1),
            method: ViewHirRequest::METHOD.to_string(),
            params: json!({
                "textDocument": { "uri": to_url(path) },
                "start": { "line": start.0, "character": start.1 },
                "end": { "line": end.0, "character": end.1 },
            }),
        },
    )
    .expect("viewHir request");
    response.result.expect("viewHir returns a result")["content"]
        .as_str()
        .expect("content is a string")
        .to_string()
}

/// The selection behaves like `viewAst`: the largest expression fully inside
/// it, not the expression enclosing it. Selecting `b` yields `b`, not `a + b`.
#[test]
fn a_selection_returns_the_largest_enclosed_expression() {
    let (db, file) = make_test_db("contract C {\n    uint x = a + b;\n}\n");
    let path = file.path(&db).to_path_buf();

    let text = view(&db, &path, (1, 17), (1, 18));
    assert!(text.contains("Ident: \"b\""), "{text}");
    assert!(!text.contains("Binary"), "{text}");

    // The whole statement encloses `a + b`, so that is the largest expression.
    let text = view(&db, &path, (1, 4), (1, 19));
    assert!(text.contains("Binary: \"+\""), "{text}");
    assert!(
        text.contains("Ident: \"a\"") && text.contains("Ident: \"b\""),
        "{text}"
    );
}

/// Containers come from every declaration the selection overlaps, so a range
/// spanning two functions reaches both bodies.
#[test]
fn a_selection_spanning_two_functions_dumps_both() {
    let (db, file) = make_test_db(
        "contract C {\n    function f() internal { uint a = 1 + 2; }\n    function g() internal { uint b = 3 * 4; }\n}\n",
    );
    let path = file.path(&db).to_path_buf();

    let text = view(&db, &path, (1, 27), (2, 43));
    assert!(text.contains("Binary: \"+\""), "{text}");
    assert!(text.contains("Binary: \"*\""), "{text}");
}

/// Selecting a whole contract descends into its members: their bodies live in
/// the members' own stores, not the contract's.
#[test]
fn a_whole_contract_selection_dumps_every_member_body() {
    let (db, file) = make_test_db(
        "contract C {\n    function f() internal { uint a = 1 + 2; }\n    function g() internal { uint b = 3 * 4; }\n}\n",
    );
    let path = file.path(&db).to_path_buf();

    let text = view(&db, &path, (0, 0), (3, 1));
    assert!(text.contains("Binary: \"+\""), "{text}");
    assert!(text.contains("Binary: \"*\""), "{text}");
}

/// An empty selection encloses no expression, so it finds nothing.
#[test]
fn an_empty_selection_finds_no_expressions() {
    let (db, file) = make_test_db("contract C {\n    uint x = a + b;\n}\n");
    let path = file.path(&db).to_path_buf();

    let text = view(&db, &path, (1, 13), (1, 13));
    assert!(text.contains("No lowered expressions found"), "{text}");
}

/// `Emit` shares its node's range with the `Call` it wraps; the wrapper wins
/// and the call is printed once, as its child.
#[test]
fn a_wrapper_and_its_call_print_once() {
    let (db, file) =
        make_test_db("contract C {\n    event E();\n    function f() internal { emit E(); }\n}\n");
    let path = file.path(&db).to_path_buf();

    let text = view(&db, &path, (2, 28), (2, 37));
    assert_eq!(text.matches("Emit").count(), 1, "{text}");
    assert_eq!(text.matches("Call").count(), 1, "{text}");
}
