use anyhow::Result;
use camino::Utf8Path;
use lsp_server::{ErrorCode, Request, Response};
use lsp_types::{
    Position, PrepareRenameResponse, RenameParams, TextDocumentPositionParams, TextEdit,
    WorkspaceEdit,
};
use rustc_hash::FxHashSet;

use crate::ast::kinds::NodeKind;
use crate::ast::{ImportId, NodeRange};
use crate::hir::exprs::Name;
use crate::hir::syntax_store::{for_each_semantic, semantic_at};
use crate::hir::types::Primitive;
use crate::ir::def_map::DefId;
use crate::ir::item_tree::ItemId;
use crate::salsa::hir_db::{def_name, owner_at};
use crate::salsa::{Db, File, RootDb};
use crate::utilities::{to_url, to_utf8path};

use super::{Identity, identity_of_semantic, lsp_range};

/// Every spelling a rename rewrites.
type Hits = FxHashSet<(File, NodeRange)>;

/// What the spelling at the cursor denotes.
pub(crate) struct Target {
    pub identity: Identity,
    pub name: Name,
}

impl Target {
    pub fn at(db: &Db, owner: DefId, range: NodeRange) -> Option<Target> {
        let id = semantic_at(db, owner, range)?;
        let identity = identity_of_semantic(db, owner, id)?;
        let text = db.text(owner.file());
        let name: Name = text.get(range.start as usize..range.end as usize)?.into();
        if is_reserved(&name) {
            return None;
        }
        let target = Target { identity, name };
        // Cannot rename a target defined in a dependency file unless it's aliased in current file
        (target.is_alias(db) || !is_dependency(db, identity.file())).then_some(target)
    }

    fn is_alias(&self, db: &Db) -> bool {
        match self.identity {
            // a namespace import aliases the whole file
            Identity::Def(DefId::File(_)) => true,
            Identity::Def(def) => def_name(db, def) != self.name.as_str(),
            _ => false,
        }
    }
}

fn is_dependency(db: &Db, file: File) -> bool {
    file.root(db).is_dependency(db)
}

pub fn prepare_rename(db: &Db, request: Request) -> Result<Response> {
    let params: TextDocumentPositionParams = serde_json::from_value(request.params)?;
    let path = to_utf8path(&params.text_document.uri)?;
    let result = locate(db, &path, params.position)
        .and_then(|(def, range)| {
            let target = Target::at(db, def, range)?;
            Some((def.file(), range, target.name))
        })
        .map(|(file, range, target)| {
            serde_json::to_value(PrepareRenameResponse::RangeWithPlaceholder {
                range: lsp_range(db, file, range),
                placeholder: target.to_string(),
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
    let Some((origin, target)) = locate(db, &path, params.text_document_position.position)
        .and_then(|(def, range)| Target::at(db, def, range).map(|target| (def.file(), target)))
    else {
        return Ok(failed(
            request.id,
            "nothing renameable at the cursor".into(),
        ));
    };
    if !is_identifier(&params.new_name) {
        return Ok(failed(
            request.id,
            format!("`{}` is not a valid Solidity identifier", params.new_name),
        ));
    }
    let hits = references(db, &target, origin);
    Ok(Response {
        id: request.id,
        result: Some(serde_json::to_value(workspace_edit(
            db,
            hits,
            &params.new_name,
        ))?),
        error: None,
    })
}

/// The owner and spelling range at the cursor — the locate both requests share.
/// `file` is not returned: it is `owner.file()` by construction. What the
/// spelling denotes is `Target::at`'s question, composed by each caller.
fn locate(db: &Db, path: &Utf8Path, position: Position) -> Option<(DefId, NodeRange)> {
    if db.is_ignored_path(path) {
        return None;
    }
    let (file, offset) = db.convert(path, position)?;
    // A renaming is non-functional in a dependency file
    if is_dependency(db, file) {
        return None;
    }

    let node = db.named_node_at(file, offset)?;
    let owner = owner_at(db, file, offset)?;
    let range = NodeRange::from(&node.node());
    let editable = matches!(
        node.node().kind_id().into(),
        NodeKind::IDENTIFIER | NodeKind::ENUM_VALUE
    );
    editable.then_some((owner, range))
}

/// Every spelling in scope that denotes `target`.
pub(crate) fn references(db: &Db, target: &Target, origin: File) -> Hits {
    let mut hits = Hits::default();
    if target.identity.is_local() {
        scan_owner(db, target.identity.def(), target, &mut hits);
    } else if target.is_alias(db) {
        scan_file(db, origin, target, &mut hits);
    } else {
        let anchor = target.identity.file();
        let mut files = db.import_map().dependents_of([anchor]);
        files.insert(anchor);
        for file in files {
            scan_file(db, file, target, &mut hits);
        }
    }
    hits
}

/// Every spelling in `file` that denotes `target`: its owners' stores and its
/// import stores.
fn scan_file(db: &Db, file: File, target: &Target, hits: &mut Hits) {
    if is_dependency(db, file) || !db.text(file).contains(target.name.as_str()) {
        return;
    }
    for owner in db.file_def_map(file).owners(file) {
        scan_owner(db, owner, target, hits);
    }
    // An import binds no name, so `owners` skips it; its store holds the specifier.
    for item in db.item_tree(file).top_level.iter() {
        if let ItemId::Import(id) = item {
            scan_owner(db, DefId::Import(ImportId { file, id: *id }), target, hits);
        }
    }
}

fn scan_owner(db: &Db, owner: DefId, target: &Target, hits: &mut Hits) {
    let file = owner.file();
    let text = db.text(file);
    for_each_semantic(db, owner, |range, id| {
        // The spelling must match: resolution alone over-matches `import {A as B}`.
        if text.get(range.start as usize..range.end as usize) != Some(target.name.as_str()) {
            return;
        }
        if identity_of_semantic(db, owner, id) == Some(target.identity) {
            hits.insert((file, range));
        }
    });
}

fn workspace_edit(db: &Db, hits: Hits, new_name: &str) -> WorkspaceEdit {
    let mut changes: std::collections::HashMap<lsp_types::Url, Vec<TextEdit>> = Default::default();
    for (file, range) in hits {
        changes
            .entry(to_url(file.path(db)))
            .or_default()
            .push(TextEdit {
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

fn is_reserved(name: &str) -> bool {
    KEYWORDS.contains(&name) || Primitive::parse(name) != Primitive::Undefined
}

const KEYWORDS: &[&str] = &[
    "abstract",
    "after",
    "address",
    "alias",
    "apply",
    "assembly",
    "auto",
    "bool",
    "break",
    "byte",
    "bytes",
    "calldata",
    "case",
    "catch",
    "constant",
    "constructor",
    "continue",
    "contract",
    "copyof",
    "days",
    "default",
    "define",
    "delete",
    "do",
    "else",
    "emit",
    "enum",
    "ether",
    "event",
    "external",
    "fallback",
    "false",
    "final",
    "for",
    "function",
    "gwei",
    "hex",
    "hours",
    "if",
    "immutable",
    "implements",
    "import",
    "in",
    "indexed",
    "inline",
    "int",
    "interface",
    "internal",
    "is",
    "let",
    "library",
    "macro",
    "mapping",
    "match",
    "memory",
    "minutes",
    "modifier",
    "mutable",
    "new",
    "null",
    "of",
    "override",
    "partial",
    "payable",
    "pragma",
    "private",
    "promise",
    "public",
    "pure",
    "receive",
    "reference",
    "relocatable",
    "return",
    "returns",
    "sealed",
    "seconds",
    "sizeof",
    "static",
    "storage",
    "string",
    "struct",
    "super",
    "supports",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "type",
    "typedef",
    "typeof",
    "uint",
    "unchecked",
    "unicode",
    "using",
    "var",
    "view",
    "virtual",
    "weeks",
    "wei",
    "while",
    "years",
];
