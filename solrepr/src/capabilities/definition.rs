use anyhow::{Context as _, Result};
use lsp_server::{Request, Response};
use lsp_types::{GotoDefinitionParams, GotoDefinitionResponse, Location as LspLocation};

use crate::ast::NodeRange;
use crate::hir::body_map::{BodyOwnerId, LocalId};
use crate::hir::syntax_store::semantic_at;
use crate::ir::def_map::DefId;
use crate::salsa::hir_db::container_at;
use crate::salsa::root_db::RootDb;
use crate::salsa::{Db, File, HirDb};
use crate::utilities::{to_url, to_utf8path};

use super::{Identity, identity_of_semantic, lsp_range};

/// Goto is a single-identity operation. A spelling that denotes several — a tied
/// call like `f(1)` between `f(uint8)` and `f(uint16)` — has no destination, so
/// it answers null instead of picking one.
pub fn definition(db: &Db, request: Request) -> Result<Response> {
    let params: GotoDefinitionParams = serde_json::from_value(request.params)?;
    let path = to_utf8path(&params.text_document_position_params.text_document.uri)?;
    if db.is_ignored_path(&path) {
        return Ok(Response {
            id: request.id,
            result: Some(serde_json::Value::Null),
            error: None,
        });
    }
    let position = params.text_document_position_params.position;
    let (file, offset) = db.convert(&path, position).context("document is not loaded")?;
    let node = db.named_node_at(file, offset).context("No node at cursor")?; //we do node_at inside context already
    let container = container_at(db, file, offset).context("no container at cursor")?;
    let range = NodeRange::from(&node.node());

    let location = semantic_at(db, container, range)
        .and_then(|id| identity_of_semantic(db, container, id))
        .and_then(|identity| location_of(db, identity));

    let result = match location {
        Some(location) => serde_json::to_value(GotoDefinitionResponse::Scalar(location))?,
        None => serde_json::Value::Null,
    };

    Ok(Response {
        id: request.id,
        result: Some(result),
        error: None,
    })
}

/// Where `identity` is declared — the one place a declaration's position is
/// read. `rename` reuses it for the declaration-site edit.
fn location_of(db: &Db, identity: Identity) -> Option<LspLocation> {
    match identity {
        // A module has no declaration node; its location is the file.
        Identity::Def(DefId::File(file)) => location_for_range(db, file, NodeRange { start: 0, end: 0 }),
        Identity::Def(def) => {
            let (file, Some(ast_id)) = def.file_and_id() else {
                return None;
            };
            let node = db.ast_id_map(file).get_node(&db.root(file), ast_id)?;
            location_for_range(db, file, NodeRange::from(&node.node()))
        }
        Identity::Local { owner, local } => location_for_range(db, owner.file(), local_range(db, owner, local)?),
        Identity::Field { owner, field } => {
            let DefId::Struct(id) = owner else {
                return None;
            };
            let _ = db.struct_data(id);
            let range = *db.source_map(owner).field_ranges.get(field)?;
            location_for_range(db, owner.file(), range)
        }
        Identity::Variant { enum_, variant } => {
            let _ = db.enum_data(enum_);
            let range = *db.source_map(DefId::Enum(enum_)).variant_ranges.get(variant)?;
            location_for_range(db, enum_.file, range)
        }
    }
}

/// A local's declaration range: body owners keep theirs in the body map, event
/// and error parameters in the item's own map.
fn local_range(db: &Db, def: DefId, local: LocalId) -> Option<NodeRange> {
    match def {
        DefId::Function(id) => db.body_source_map(BodyOwnerId::Function(id)).local_ranges.get(local).copied(),
        DefId::Modifier(id) => db.body_source_map(BodyOwnerId::Modifier(id)).local_ranges.get(local).copied(),
        DefId::Event(_) | DefId::Error(_) => db.source_map(def).local_ranges.get(local).copied(),
        _ => None,
    }
}

fn location_for_range(db: &Db, file: File, range: NodeRange) -> Option<LspLocation> {
    Some(LspLocation {
        uri: to_url(file.path(db)),
        range: lsp_range(db, file, range),
    })
}

#[cfg(test)]
mod tests {
    use crate::utilities::byte_to_position;
    use lsp_types::Position;
    use ropey::Rope;

    #[test]
    fn converts_byte_offsets_to_utf16_positions() {
        let rope = Rope::from_str("αx\nvalue");
        assert_eq!(byte_to_position(&rope, 2), Position { line: 0, character: 1 });
        assert_eq!(byte_to_position(&rope, 3), Position { line: 0, character: 2 });
        assert_eq!(byte_to_position(&rope, 4), Position { line: 1, character: 0 });
    }
}
