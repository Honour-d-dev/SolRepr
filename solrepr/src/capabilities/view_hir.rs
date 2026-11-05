use std::collections::HashMap;
use std::fmt::Write;

use anyhow::Context as _;
use lsp_server::{Request, Response};
use lsp_types::{Position, TextDocumentIdentifier};

use crate::ast::NodeRange;
use crate::hir::body_map::BodyOwnerId;
use crate::hir::exprs::{Expr, ExprId, Literal};
use crate::hir::syntax_store::{SourceMap, SyntaxStore};
use crate::ir::def_map::DefId;
use crate::salsa::{HirDb, Db};
use crate::utilities::{log_info, to_utf8path};

/// Custom LSP request: `solidity/viewHir`
///
/// Params: `{ textDocument, start, end }` — two LSP positions delimiting
/// the user's selection.
///
/// Returns: `{ content: string }` — a tree dump of the lowered HIR
/// expressions whose source range overlaps the selection, rendered
/// similarly to `print_named_tree` but over the `Expr` enum.
///
/// Works on any item that has an `SyntaxStore` — function/modifier bodies,
/// state variable initializers, struct fields, contract bases, etc.
pub fn view_hir(db: &Db, request: Request) -> anyhow::Result<Response> {
    let params: ViewHirParams = serde_json::from_value(request.params)?;
    let path = to_utf8path(&params.text_document.uri)?;
    log_info(format!("viewHir request for {path}"));

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

    let containers = db.enclosing_containers(
        db.named_node_at(file, start_off)
            .context("No node at selection start")?
            .node(),
        file,
    );

    let container = containers.last().context("No enclosing container found")?;

    // Process each SyntaxStore that belongs to this container.
    // Functions and modifiers have two stores (item-level + body-level);
    // other items have one.
    let mut out = String::new();
    let mut found_any = false;

    macro_rules! process_store {
        ($store:expr, $map:expr) => {
            let store: &SyntaxStore = $store;
            let ranges = build_expr_ranges(&$map);
            let roots = find_root_exprs(store, &ranges, selection);
            if !roots.is_empty() {
                found_any = true;
                for root in roots {
                    print_expr_tree(root, store, &ranges, 0, &mut out);
                }
            }
        };
    }

    match container {
        DefId::Function(id) => {
            let fn_data = db.function_data(*id);
            let body = db.body_map(BodyOwnerId::Function(*id));
            process_store!(
                &fn_data.syntax_store,
                db.item_source_map(DefId::Function(*id))
            );
            process_store!(
                &body.syntax_store,
                db.body_source_map(BodyOwnerId::Function(*id))
            );
        }
        DefId::Modifier(id) => {
            let mod_data = db.modifier_data(*id);
            let body = db.body_map(BodyOwnerId::Modifier(*id));
            process_store!(
                &mod_data.syntax_store,
                db.item_source_map(DefId::Modifier(*id))
            );
            process_store!(
                &body.syntax_store,
                db.body_source_map(BodyOwnerId::Modifier(*id))
            );
        }
        DefId::Var(id) => {
            let data = db.var_data(*id);
            process_store!(&data.syntax_store, db.item_source_map(DefId::Var(*id)));
        }
        DefId::Struct(id) => {
            let data = db.struct_data(*id);
            process_store!(&data.syntax_store, db.item_source_map(DefId::Struct(*id)));
        }
        DefId::Enum(id) => {
            let data = db.enum_data(*id);
            process_store!(&data.syntax_store, db.item_source_map(DefId::Enum(*id)));
        }
        DefId::Event(id) => {
            let data = db.event_data(*id);
            process_store!(&data.syntax_store, db.item_source_map(DefId::Event(*id)));
        }
        DefId::Error(id) => {
            let data = db.error_data(*id);
            process_store!(&data.syntax_store, db.item_source_map(DefId::Error(*id)));
        }
        DefId::Contract(id) => {
            let data = db.contract_data(*id);
            process_store!(&data.syntax_store, db.item_source_map(DefId::Contract(*id)));
        }
        DefId::Interface(id) => {
            let data = db.interface_data(*id);
            process_store!(&data.syntax_store, db.item_source_map(DefId::Interface(*id)));
        }
        DefId::Library(id) => {
            let data = db.library_data(*id);
            process_store!(&data.syntax_store, db.item_source_map(DefId::Library(*id)));
        }
        DefId::Udvt(id) => {
            let data = db.udvt_data(*id);
            process_store!(&data.syntax_store, db.item_source_map(DefId::Udvt(*id)));
        }
        DefId::Using(id) => {
            let data = db.using_data(*id);
            process_store!(&data.syntax_store, db.item_source_map(DefId::Using(*id)));
        }
        DefId::Import(id) => {
            let data = db.import_data(*id);
            process_store!(&data.syntax_store, db.item_source_map(DefId::Import(*id)));
        }
        DefId::File(_) => {}
    }

    if !found_any {
        writeln!(out, "No lowered expressions found in the selected range.").ok();
    }

    let result = serde_json::json!({ "content": out });
    Ok(Response {
        id: request.id,
        result: Some(result),
        error: None,
    })
}

// ── Request type ─────────────────────────────────────────────────────

pub struct ViewHirRequest;

impl lsp_types::request::Request for ViewHirRequest {
    type Params = ViewHirParams;
    type Result = ViewHirResult;
    const METHOD: &'static str = "solidity/viewHir";
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct ViewHirResult {
    pub content: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct ViewHirParams {
    #[serde(rename = "textDocument")]
    pub text_document: TextDocumentIdentifier,
    pub start: Position,
    pub end: Position,
}

// ── ExprId → NodeRange mapping ───────────────────────────────────────

/// Build a map from `ExprId` to its source `NodeRange`.
fn build_expr_ranges(map: &SourceMap) -> HashMap<ExprId, NodeRange> {
    map
        .expr_ranges
        .iter()
        .map(|(id, range)| (id, *range))
        .collect()
}

// ── Tree printing ────────────────────────────────────────────────────

/// Recursively print an `Expr` and its children as an indented tree,
/// mirroring `print_named_tree`'s style.
fn print_expr_tree(
    id: ExprId,
    store: &SyntaxStore,
    ranges: &HashMap<ExprId, NodeRange>,
    indent: usize,
    out: &mut String,
) {
    let expr = &store.exprs[id];
    let pad = "  ".repeat(indent);
    let loc = ranges
        .get(&id)
        .map_or(String::new(), |r| format!("  @{:?}..{:?}", r.start, r.end));

    match expr {
        Expr::Ident(name) => {
            writeln!(out, "{pad}Ident: \"{name}\"{loc}").ok();
        }
        Expr::Path(p) => {
            writeln!(out, "{pad}Path: \"{p}\"{loc}").ok();
        }
        Expr::Literal(lit) => {
            let (kind, val) = match lit {
                Literal::Boolean(b) => ("bool", b.to_string()),
                Literal::Number(n) => ("number", n.to_string()),
                Literal::String(s) => ("string", s.to_string()),
                Literal::HexString(s) => ("hexstring", s.to_string()),
            };
            writeln!(out, "{pad}Literal({kind}): \"{val}\"{loc}").ok();
        }
        Expr::Binary { op, left, right } => {
            writeln!(out, "{pad}Binary: \"{op}\"{loc}").ok();
            print_expr_tree(*left, store, ranges, indent + 1, out);
            print_expr_tree(*right, store, ranges, indent + 1, out);
        }
        Expr::Unary { op, arg } => {
            writeln!(out, "{pad}Unary: \"{}\"{loc}", op.as_str()).ok();
            print_expr_tree(*arg, store, ranges, indent + 1, out);
        }
        Expr::Update { op, arg } => {
            writeln!(out, "{pad}Update: \"{}\"{loc}", op.as_str()).ok();
            print_expr_tree(*arg, store, ranges, indent + 1, out);
        }
        Expr::Ternary { cond, then, els } => {
            writeln!(out, "{pad}Ternary{loc}").ok();
            print_expr_tree(*cond, store, ranges, indent + 1, out);
            print_expr_tree(*then, store, ranges, indent + 1, out);
            print_expr_tree(*els, store, ranges, indent + 1, out);
        }
        Expr::Assign { lhs, rhs, op } => {
            let op = op.map_or("=", |op| op.as_str());
            writeln!(out, "{pad}Assign: \"{op}\"{loc}").ok();
            print_expr_tree(*lhs, store, ranges, indent + 1, out);
            print_expr_tree(*rhs, store, ranges, indent + 1, out);
        }
        Expr::Member { obj, prop } => {
            writeln!(out, "{pad}Member: \".{prop}\"{loc}").ok();
            print_expr_tree(*obj, store, ranges, indent + 1, out);
        }
        Expr::Call { callee, args } => {
            writeln!(out, "{pad}Call{loc}").ok();
            print_expr_tree(*callee, store, ranges, indent + 1, out);
            for arg in args.iter() {
                print_expr_tree(*arg, store, ranges, indent + 1, out);
            }
        }
        Expr::ModifierInv(call) => {
            writeln!(out, "{pad}ModifierInvocation{loc}").ok();
            print_expr_tree(*call, store, ranges, indent + 1, out);
        }
        Expr::Emit(call) => {
            writeln!(out, "{pad}Emit{loc}").ok();
            print_expr_tree(*call, store, ranges, indent + 1, out);
        }
        Expr::Revert(call) => {
            writeln!(out, "{pad}Revert{loc}").ok();
            print_expr_tree(*call, store, ranges, indent + 1, out);
        }
        Expr::Return(value) => {
            writeln!(out, "{pad}Return{loc}").ok();
            if let Some(value) = value {
                print_expr_tree(*value, store, ranges, indent + 1, out);
            }
        }
        Expr::CallOptions { callee, options } => {
            writeln!(out, "{pad}CallOptions{loc}").ok();
            print_expr_tree(*callee, store, ranges, indent + 1, out);
            for (name, value) in options.iter() {
                writeln!(out, "{pad}  option: \"{name}\"").ok();
                print_expr_tree(*value, store, ranges, indent + 2, out);
            }
        }
        Expr::ArrayAccess { base, index } => {
            writeln!(out, "{pad}ArrayAccess{loc}").ok();
            print_expr_tree(*base, store, ranges, indent + 1, out);
            if let Some(idx) = index {
                print_expr_tree(*idx, store, ranges, indent + 1, out);
            }
        }
        Expr::Slice { base, from, to } => {
            writeln!(out, "{pad}Slice{loc}").ok();
            print_expr_tree(*base, store, ranges, indent + 1, out);
            if let Some(from) = from {
                print_expr_tree(*from, store, ranges, indent + 1, out);
            }
            if let Some(to) = to {
                print_expr_tree(*to, store, ranges, indent + 1, out);
            }
        }
        Expr::InlineArray(elems) => {
            writeln!(out, "{pad}InlineArray{loc}").ok();
            for elem in elems.iter() {
                print_expr_tree(*elem, store, ranges, indent + 1, out);
            }
        }
        Expr::Tuple(elems) => {
            writeln!(out, "{pad}Tuple{loc}").ok();
            for elem in elems.iter() {
                match elem {
                    Some(elem) => print_expr_tree(*elem, store, ranges, indent + 1, out),
                    // An omitted component — `(a, , b)`.
                    None => {
                        writeln!(out, "{pad}  <hole>").ok();
                    }
                }
            }
        }
        Expr::MetaType(inner) => {
            writeln!(out, "{pad}MetaType{loc}").ok();
            print_expr_tree(*inner, store, ranges, indent + 1, out);
        }
        Expr::New { ty } => {
            writeln!(out, "{pad}New{loc}").ok();
            print_expr_tree(*ty, store, ranges, indent + 1, out);
        }
        Expr::NamedArgs(named) => {
            writeln!(out, "{pad}NamedArgs{loc}").ok();
            for (name, value) in named.iter() {
                writeln!(out, "{pad}  name: \"{name}\"").ok();
                print_expr_tree(*value, store, ranges, indent + 2, out);
            }
        }
    }
}

// ── Root expression discovery ────────────────────────────────────────

/// Find the outermost expressions whose source range overlaps the
/// selection. An expression is a "root" if it overlaps the selection
/// and no other overlapping expression contains it.
fn find_root_exprs(
    store: &SyntaxStore,
    ranges: &HashMap<ExprId, NodeRange>,
    selection: NodeRange,
) -> Vec<ExprId> {
    // Collect (id, range) for all expressions that overlap the selection.
    let overlapping: Vec<(ExprId, NodeRange)> = store
        .exprs
        .iter()
        .filter_map(|(id, _)| {
            let r = ranges.get(&id)?;
            ranges_overlap(r, &selection).then_some((id, *r))
        })
        .collect();

    // Keep only those not contained by any other overlapping expression.
    let mut roots: Vec<ExprId> = overlapping
        .iter()
        .filter(|(_, my_range)| {
            !overlapping
                .iter()
                .any(|(_, other_range)| other_range.contains(my_range) && other_range != my_range)
        })
        .map(|(id, _)| *id)
        .collect();

    roots.sort_by_key(|id| ranges.get(id).map(|r| r.start).unwrap_or(u32::MAX));
    roots
}

fn ranges_overlap(a: &NodeRange, b: &NodeRange) -> bool {
    a.start < b.end && b.start < a.end
}
