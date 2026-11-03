use anyhow::Context as _;
use lsp_server::{Request, Response};
use lsp_types::{Position, TextDocumentIdentifier};
use tree_sitter::Node;

use crate::ast::NodeRange;
use crate::salsa::{RootDb, Db};
use crate::utilities::{log_info, print_named_tree, to_utf8path};

/// Custom LSP request: `solidity/viewAst`
///
/// Params: `{ textDocument, start, end }` — two LSP positions delimiting
/// the user's selection.
///
/// Returns: `{ content: string }` — the raw tree-sitter named AST tree
/// for the largest named node(s) fully enclosed by the selection,
/// produced by `print_named_tree`.
pub fn view_ast(db: &Db, request: Request) -> anyhow::Result<Response> {
    let params: ViewAstParams = serde_json::from_value(request.params)?;
    let path = to_utf8path(&params.text_document.uri)?;

    let (file, start_off) = db
        .convert(&path, params.start)
        .context("document is not loaded")?;
    let (_, end_off) = db
        .convert(&path, params.end)
        .context("document is not loaded")?;
    let selection = NodeRange {
        start: start_off,
        end: end_off,
    };
    log_info(format!("viewAst request for {path} at {selection:?}"));

    let ast = db.ast(file);
    let root = ast.root();
    let source = ast.source();

    // Find the largest named node(s) fully enclosed by the selection.
    // tree-sitter only provides `descendant_for_byte_range` (smallest
    // node *enclosing* the range), so we do a manual top-down walk:

    // let result = find_enclosed_node(root.node(), selection)
    //     .map(|n| serde_json::json!({ "content": print_named_tree(&mut n.walk(), source, 0) }));

    let nodes = find_enclosed_nodes(root.node(), selection)
        .into_iter()
        .map(|n| print_named_tree(&mut n.walk(), source, 0))
        .collect::<Vec<_>>()
        .join("\n");
    log_info(format!("found {} nodes", nodes.len()));

    Ok(Response {
        id: request.id,
        result: Some(serde_json::json!({ "content": nodes })),
        error: None,
    })
}

// ── Node discovery ───────────────────────────────────────────────────

/// Find the largest named nodes whose range is fully contained within
/// `selection`. Walks top-down; once a node is fully contained, its
/// children are not visited (they'd be smaller and redundant).
fn find_enclosed_nodes(node: Node, selection: NodeRange) -> Vec<Node> {
    let range = NodeRange::from(&node);

    if selection.contains(&range) {
        return vec![node];
    }

    // No overlap at all → skip children.
    if range.end <= selection.start || range.start >= selection.end {
        return vec![];
    }

    // Partially overlapping → recurse into children.
    let mut nodes = vec![];
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        nodes.extend(find_enclosed_nodes(child, selection));
    }
    nodes
}

// ── Request type ─────────────────────────────────────────────────────

pub struct ViewAstRequest;

impl lsp_types::request::Request for ViewAstRequest {
    type Params = ViewAstParams;
    type Result = ViewAstResult;
    const METHOD: &'static str = "solidity/viewAst";
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct ViewAstResult {
    pub content: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct ViewAstParams {
    #[serde(rename = "textDocument")]
    pub text_document: TextDocumentIdentifier,
    pub start: Position,
    pub end: Position,
}
