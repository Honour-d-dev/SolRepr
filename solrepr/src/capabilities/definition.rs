use anyhow::{Context as _, Result};
use lsp_server::{Request, Response};
use lsp_types::{GotoDefinitionParams, GotoDefinitionResponse, Location as LspLocation, Range};

use crate::ast::NodeRange;
use crate::hir::body_map::{BodyOwnerId, LocalId};
use crate::hir::item_data::{FieldId, SemanticId, VariantId};
use crate::hir::resolution::Resolution;
use crate::hir::types::Ty;
use crate::ir::def_map::DefId;
use crate::hir::syntax_store::semantic_at;
use crate::salsa::hir_db::owner_at;
use crate::salsa::root_db::RootDb;
use crate::salsa::{File, HirDb, Db};
use crate::utilities::{byte_to_position, to_url, to_utf8path};

enum DefinitionTarget {
    Def(DefId),
    Range(File, NodeRange),
    File(File),
}

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
    let (file, offset) = db
        .convert(&path, position)
        .context("document is not loaded")?;
    let node = db
        .named_node_at(file, offset)
        .context("No node at cursor")?; //we do node_at inside context already
    let owner = owner_at(db, file, offset).context("no owner at cursor")?;
    let range = NodeRange::from(&node.node());

    let locations = semantic_at(db, owner, range)
        .map(|id| semantic_targets(db, owner, id))
        .unwrap_or_default()
        .into_iter()
        .flat_map(|target| locations_for_target(db, target))
        .collect::<Vec<_>>();

    let result = match locations.as_slice() {
        [] => serde_json::Value::Null,
        [location] => serde_json::to_value(GotoDefinitionResponse::Scalar(location.clone()))?,
        _ => serde_json::to_value(GotoDefinitionResponse::Array(locations))?,
    };

    Ok(Response {
        id: request.id,
        result: Some(result),
        error: None,
    })
}

fn semantic_targets(db: &Db, owner: DefId, id: SemanticId) -> Vec<DefinitionTarget> {
    match id {
        SemanticId::Expr(expr) => db
            .inference(owner)
            .expr_resolution(expr)
            .map(|resolution| resolution_targets(db, resolution.clone()))
            .unwrap_or_default(),
        SemanticId::Local(local) => local_targets(db, owner, local),
        SemanticId::Field(field) => field_targets(db, owner, field),
        SemanticId::Variant(variant) => variant_targets(db, owner, variant),
        SemanticId::Type(ty) => ty_targets(db, db.inference(owner).ty(ty).ty()),
    }
}

fn resolution_targets(db: &Db, resolution: Resolution) -> Vec<DefinitionTarget> {
    match resolution {
        Resolution::Local(local) => local_targets(db, local.owner, local.id),
        Resolution::Var(def) | Resolution::This(def) => vec![DefinitionTarget::Def(def)],
        Resolution::Callable(callable) => callable
            .def()
            .map(|def| vec![DefinitionTarget::Def(def)])
            .unwrap_or_default(),
        Resolution::Callables(callables) => callables
            .iter()
            .filter_map(|callable| callable.def().map(DefinitionTarget::Def))
            .collect(),
        Resolution::Type(ty) => ty_targets(db, ty.ty()),
        Resolution::Module(file) => vec![DefinitionTarget::File(file)],
        Resolution::MetaType(ty) => ty_targets(db, ty.ty()),
        Resolution::Literal(_) | Resolution::Builtin(_) => Vec::new(),
        Resolution::Field(field) => field
            .struct_field()
            .map(|(owner, field)| field_targets(db, owner, field))
            .unwrap_or_default(),
        Resolution::Variant(variant) => {
            variant_targets(db, DefId::Enum(variant.enum_id), variant.id)
        }
        Resolution::Super(def) => db
            .bases(def)
            .get(1)
            .copied()
            .map(|base| vec![DefinitionTarget::Def(base)])
            .unwrap_or_default(),
    }
}

fn local_targets(db: &Db, owner: DefId, local_id: LocalId) -> Vec<DefinitionTarget> {
    let (file, _) = owner.file_and_id();
    let range = match owner {
        DefId::Function(id) => db
            .body_source_map(BodyOwnerId::Function(id))
            .local_ranges
            .get(local_id)
            .copied(),
        DefId::Modifier(id) => db
            .body_source_map(BodyOwnerId::Modifier(id))
            .local_ranges
            .get(local_id)
            .copied(),
        DefId::Event(_) | DefId::Error(_) => db
            .source_map(owner)
            .local_ranges
            .get(local_id)
            .copied(),
        _ => return Vec::new(),
    };
    let Some(range) = range else {
        return Vec::new();
    };
    vec![DefinitionTarget::Range(file, range)]
}

fn field_targets(db: &Db, owner: DefId, field_id: FieldId) -> Vec<DefinitionTarget> {
    let DefId::Struct(struct_id) = owner else {
        return Vec::new();
    };
    let _ = db.struct_data(struct_id);
    let Some(range) = db.source_map(owner).field_ranges.get(field_id).copied() else {
        return Vec::new();
    };
    vec![DefinitionTarget::Range(owner.file_and_id().0, range)]
}

fn variant_targets(db: &Db, owner: DefId, variant_id: VariantId) -> Vec<DefinitionTarget> {
    let DefId::Enum(enum_id) = owner else {
        return Vec::new();
    };
    let _ = db.enum_data(enum_id);
    let Some(range) = db
        .source_map(owner)
        .variant_ranges
        .get(variant_id)
        .copied()
    else {
        return Vec::new();
    };
    vec![DefinitionTarget::Range(owner.file_and_id().0, range)]
}

fn ty_targets(db: &Db, ty: Ty) -> Vec<DefinitionTarget> {
    match ty.def_id(db) {
        Some(DefId::File(file)) => vec![DefinitionTarget::File(file)],
        Some(def) => vec![DefinitionTarget::Def(def)],
        None => Vec::new(),
    }
}

fn locations_for_target(db: &Db, target: DefinitionTarget) -> Option<LspLocation> {
    match target {
        DefinitionTarget::Def(def) => {
            let (file, Some(ast_id)) = def.file_and_id() else {
                return None;
            };
            let node = db.ast_id_map(file).get_node(&db.root(file), ast_id)?;
            location_for_range(db, file, NodeRange::from(&node.node()))
        }
        DefinitionTarget::Range(file, range) => location_for_range(db, file, range),
        DefinitionTarget::File(file) => {
            location_for_range(db, file, NodeRange { start: 0, end: 0 })
        }
    }
}

fn location_for_range(db: &Db, file: File, range: NodeRange) -> Option<LspLocation> {
    let uri = to_url(file.path(db));
    let rope = db.rope(file);
    Some(LspLocation {
        uri,
        range: Range {
            start: byte_to_position(&rope, range.start as usize),
            end: byte_to_position(&rope, range.end as usize),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::Position;
    use ropey::Rope;

    #[test]
    fn converts_byte_offsets_to_utf16_positions() {
        let rope = Rope::from_str("αx\nvalue");
        assert_eq!(
            byte_to_position(&rope, 2),
            Position {
                line: 0,
                character: 1
            }
        );
        assert_eq!(
            byte_to_position(&rope, 3),
            Position {
                line: 0,
                character: 2
            }
        );
        assert_eq!(
            byte_to_position(&rope, 4),
            Position {
                line: 1,
                character: 0
            }
        );
    }
}
