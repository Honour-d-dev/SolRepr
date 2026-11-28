use anyhow::Result;
use lsp_server::{Request, Response};
use lsp_types::{Location, Position, ReferenceParams};
use rustc_hash::FxHashSet;

use crate::ast::kinds::NodeKind;
use crate::ast::{ImportId, NodeRange};
use crate::hir::exprs::Name;
use crate::hir::syntax_store::{for_each_semantic, semantic_at};
use crate::hir::types::Primitive;
use crate::ir::def_map::DefId;
use crate::ir::item_tree::ItemId;
use crate::paths::NormalizedPath;
use crate::salsa::hir_db::{container_at, def_name};
use crate::salsa::root_db::def_range;
use crate::salsa::{Db, File, HirDb, RootDb};

use super::definition::local_range;
use super::{Identity, identity_of_semantic, lsp_range};

/// Every spelling a rename or references request reports.
pub(crate) type Hits = FxHashSet<(File, NodeRange)>;

/// What the spelling at the cursor denotes.
pub(crate) struct Target {
    pub identity: Identity,
    pub name: Name,
}

impl Target {
    pub fn at(db: &Db, def: DefId, range: NodeRange) -> Option<Target> {
        let id = semantic_at(db, def, range)?;
        let identity = identity_of_semantic(db, def, id)?;
        let text = db.text(def.file());
        let name: Name = text.get(range.start as usize..range.end as usize)?.into();
        (!is_reserved(&name)).then_some(Target { identity, name })
    }

    pub(crate) fn is_alias(&self, db: &Db) -> bool {
        match self.identity {
            // a namespace import aliases the whole file
            Identity::Def(DefId::File(_)) => true,
            Identity::Def(def) => def_name(db, def) != self.name.as_str(),
            _ => false,
        }
    }
}

/// The def and spelling range at the cursor — the locate both requests share.
/// `file` is not returned: it is `def.file()` by construction. What the
/// spelling denotes is `Target::at`'s question, composed by each caller.
pub(crate) fn locate(db: &Db, path: &NormalizedPath, position: Position) -> Option<(DefId, NodeRange)> {
    if db.is_ignored_path(path) {
        return None;
    }
    let (file, offset) = db.convert(path, position)?;
    let node = db.named_node_at(file, offset)?;
    let def = container_at(db, file, offset)?;
    let range = NodeRange::from(&node.node());
    let editable = matches!(node.node().kind_id().into(), NodeKind::IDENTIFIER | NodeKind::ENUM_VALUE);
    editable.then_some((def, range))
}

/// Every spelling in scope that denotes `target`.
pub(crate) fn occurrences(db: &Db, target: &Target, origin: File) -> Hits {
    let mut hits = Hits::default();
    if target.identity.is_local() {
        scan_def(db, target.identity.def(), target, &mut hits);
    } else if target.is_alias(db) {
        scan_file(db, origin, target, &mut hits);
    } else {
        let anchor = target.identity.file();
        for file in std::iter::once(anchor).chain(db.dependents_of(anchor).iter().copied()) {
            scan_file(db, file, target, &mut hits);
        }
    }
    hits
}

/// Every spelling in `file` that denotes `target`: its defs' stores and its
/// import stores.
fn scan_file(db: &Db, file: File, target: &Target, hits: &mut Hits) {
    if !db.text(file).contains(target.name.as_str()) {
        return;
    }
    for def in db.file_def_map(file).file_defs(file) {
        scan_def(db, def, target, hits);
    }
    // An import binds no name, so `file_defs` skips it; its store holds the specifier.
    for item in db.item_tree(file).top_level.iter() {
        if let ItemId::Import(id) = item {
            scan_def(db, DefId::Import(ImportId { file, id: *id }), target, hits);
        }
    }
}

fn scan_def(db: &Db, def: DefId, target: &Target, hits: &mut Hits) {
    let file = def.file();
    let text = db.text(file);
    for_each_semantic(db, def, |range, id| {
        // The spelling must match: resolution alone over-matches `import {A as B}`.
        if text.get(range.start as usize..range.end as usize) != Some(target.name.as_str()) {
            return;
        }
        if identity_of_semantic(db, def, id) == Some(target.identity) {
            hits.insert((file, range));
        }
    });
}

pub fn references(db: &Db, request: Request) -> Result<Response> {
    let params: ReferenceParams = serde_json::from_value(request.params)?;
    let path = NormalizedPath::try_from(&params.text_document_position.text_document.uri)?;
    let result = locate(db, &path, params.text_document_position.position)
        .and_then(|(def, range)| {
            let target = Target::at(db, def, range)?;
            let mut hits = occurrences(db, &target, def.file());
            if !params.context.include_declaration
                && let Some(declaration) = declaration_range(db, target.identity)
            {
                hits.remove(&declaration);
            }
            Some(hits)
        })
        .map(|hits| {
            let mut locations: Vec<Location> = hits
                .into_iter()
                .map(|(file, range)| Location {
                    uri: file.path(db).to_url(),
                    range: lsp_range(db, file, range),
                })
                .collect();
            locations.sort_by(|a, b| {
                (a.uri.as_str(), a.range.start.line, a.range.start.character).cmp(&(
                    b.uri.as_str(),
                    b.range.start.line,
                    b.range.start.character,
                ))
            });
            locations
        });

    Ok(Response {
        id: request.id,
        result: Some(serde_json::to_value(result)?),
        error: None,
    })
}

/// Where `identity` is declared, if it has a declaration site.
fn declaration_range(db: &Db, identity: Identity) -> Option<(File, NodeRange)> {
    match identity {
        // A module has no declaration node; its location is the file.
        Identity::Def(DefId::File(_)) => None,
        Identity::Def(def) => def_range(db, def).map(|range| (def.file(), range)),
        Identity::Local { owner, local } => local_range(db, owner, local).map(|range| (owner.file(), range)),
        Identity::Field { owner, field } => {
            let DefId::Struct(id) = owner else {
                return None;
            };
            let _ = db.struct_data(id);
            db.source_map(owner)
                .field_ranges
                .get(field)
                .copied()
                .map(|range| (owner.file(), range))
        }
        Identity::Variant { enum_, variant } => {
            let _ = db.enum_data(enum_);
            db.source_map(DefId::Enum(enum_))
                .variant_ranges
                .get(variant)
                .copied()
                .map(|range| (enum_.file, range))
        }
    }
}

pub(crate) fn is_reserved(name: &str) -> bool {
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
