//! A package whose source roots import each other: legal Solidity, and a
//! def-map cycle, because collecting one root reads the other's map.

use crate::capabilities::diagnostics::lsp_diagnostics;
use crate::hir::diagnostics::SemanticDiagnostic;
use crate::salsa::{Db, RootDb};
use crate::utilities::fixture_path;
use crate::workspace::discover_workspaces;

/// `src/A.sol` imports `test/B.sol` and `test/B.sol` imports `src/A.sol`, so the
/// two roots' def maps are mutually recursive. Collecting either one used to
/// panic; it now iterates to a fixed point and resolves both imports.
#[test]
fn roots_that_import_each_other_converge() {
    let root = fixture_path("fixtures/workspaces/cross_roots");
    let workspace = discover_workspaces(std::slice::from_ref(&root));
    assert_eq!(workspace.roots.len(), 2, "src and test are separate roots");

    let db = Db::new(workspace);
    let a = db.file(&root.join("src/A.sol")).expect("A is indexed");

    // The path the crash came through: diagnostics collect A's root, which
    // reaches into B's root, which reaches back into A's.
    let _ = lsp_diagnostics(&db, a);

    for relative in ["src/A.sol", "test/B.sol"] {
        let file = db.file(&root.join(relative)).expect("indexed");
        let root_id = db.file_source_root(file).expect("file has a root");
        let unresolved = db
            .root_def_map_analysis(root_id)
            .diagnostics
            .iter()
            .filter(|diagnostic| matches!(diagnostic, SemanticDiagnostic::UnresolvedImport { .. }))
            .count();
        assert_eq!(unresolved, 0, "{relative} resolves its cross-root import");
    }
}
