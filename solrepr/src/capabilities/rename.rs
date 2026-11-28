use anyhow::Result;
use camino::Utf8Path;
use lsp_server::{ErrorCode, Request, Response};
use lsp_types::{Position, PrepareRenameResponse, RenameParams, TextDocumentPositionParams, TextEdit, WorkspaceEdit};

use crate::ast::NodeRange;
use crate::ir::def_map::DefId;
use crate::salsa::{Db, File};
use crate::utilities::{to_url, to_utf8path};

use super::lsp_range;
use super::references::{Hits, Target, is_reserved, locate, occurrences};

pub fn prepare_rename(db: &Db, request: Request) -> Result<Response> {
    let params: TextDocumentPositionParams = serde_json::from_value(request.params)?;
    let path = to_utf8path(&params.text_document.uri)?;
    let result = rename_target(db, &path, params.position)
        .map(|(def, range, target)| {
            serde_json::to_value(PrepareRenameResponse::RangeWithPlaceholder {
                range: lsp_range(db, def.file(), range),
                placeholder: target.name.to_string(),
            })
        })
        .transpose()?
        .unwrap_or(serde_json::Value::Null);
    Ok(Response {
        id: request.id,
        result: Some(result),
        error: None,
    })
}

pub fn rename(db: &Db, request: Request) -> Result<Response> {
    let params: RenameParams = serde_json::from_value(request.params)?;
    let path = to_utf8path(&params.text_document_position.text_document.uri)?;
    let Some((origin, target)) =
        rename_target(db, &path, params.text_document_position.position).map(|(def, _, target)| (def.file(), target))
    else {
        return Ok(failed(request.id, "nothing renameable at the cursor".into()));
    };
    if !is_identifier(&params.new_name) {
        return Ok(failed(
            request.id,
            format!("`{}` is not a valid Solidity identifier", params.new_name),
        ));
    }
    let hits = occurrences(db, &target, origin);
    Ok(Response {
        id: request.id,
        result: Some(serde_json::to_value(workspace_edit(db, hits, &params.new_name))?),
        error: None,
    })
}

fn is_dependency(db: &Db, file: File) -> bool {
    file.root(db).is_dependency(db)
}

/// The cursor target, plus rename's writability gate: a declaration in a
/// dependency root is read-only, unless the spelling is a file-local alias.
pub(crate) fn rename_target(db: &Db, path: &Utf8Path, position: Position) -> Option<(DefId, NodeRange, Target)> {
    let (def, range) = locate(db, path, position)?;
    if is_dependency(db, def.file()) {
        return None;
    }
    let target = Target::at(db, def, range)?;
    (target.is_alias(db) || !is_dependency(db, target.identity.file())).then_some((def, range, target))
}

fn workspace_edit(db: &Db, hits: Hits, new_name: &str) -> WorkspaceEdit {
    let mut changes: std::collections::HashMap<lsp_types::Url, Vec<TextEdit>> = Default::default();
    for (file, range) in hits {
        changes.entry(to_url(file.path(db))).or_default().push(TextEdit {
            range: lsp_range(db, file, range),
            new_text: new_name.to_string(),
        });
    }
    for edits in changes.values_mut() {
        edits.sort_by_key(|edit| (edit.range.start.line, edit.range.start.character));
    }
    WorkspaceEdit {
        changes: Some(changes),
        ..Default::default()
    }
}

fn failed(id: lsp_server::RequestId, message: String) -> Response {
    Response::new_err(id, ErrorCode::RequestFailed as i32, message)
}

pub(crate) fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_' || first == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
        && !is_reserved(name)
}
