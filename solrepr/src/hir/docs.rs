use rustc_hash::FxHashSet;
use triomphe::Arc;

use crate::ast::kinds::NodeKind;
use crate::ast::{AstNode, NodeRange};
use crate::hir::signatures::signature_of;
use crate::ir::def_map::{DefId, DefNamespace};
use crate::salsa::File;
use crate::salsa::interned_db::Id;
use crate::salsa::root_db::RootDb;
use crate::salsa::HirDb;

#[salsa::tracked(lru = 100)]
pub fn docs<'db>(db: &'db dyn HirDb, def: Id<'db>) -> Option<String> {
    doc_tags(db, def).map(|tags| format_doc(&tags))
}

/// Parsed NatSpec for a def with `@inheritdoc` expanded against the
/// linearized bases. Tracked so doc chains are incremental: `D.f` inherits
/// `B.f` inherits `A.f` is three memoized queries, and the expansion rides
/// `bases`'s cycle-free MRO so it can never loop.
#[salsa::tracked(lru = 100)]
fn doc_tags<'db>(db: &'db dyn HirDb, def: Id<'db>) -> Option<Arc<DocTags>> {
    let def_id = def.id(db);
    let (file, erased_id) = def_id.file_and_id();
    let ast_id_map = db.ast_id_map(file);
    let root = db.root(file);
    let node = ast_id_map.get_node(&root, erased_id?)?;
    let raw = collect_doc_comments(&root, &node)?;
    let mut doc = parse_doc(&raw);

    if let Some(base_name) = doc.inherit.take()
        && let Some(target) = inherit_target(db, def_id, &base_name)
        && let Some(base_doc) = doc_tags(db, Id::new(db, target))
    {
        doc.merge_missing(&base_doc);
    }
    Some(Arc::new(doc))
}

/// The same-named member of the base `base_name` refers to, for `@inheritdoc`.
/// `def`'s contract is found through `owner`; `base_name` is matched against
/// the linearized bases (`bases[0]` is the contract itself — skipped). The
/// best same-namespace candidate wins on the number of positionally-equal
/// parameter types, earlier declarations breaking ties.
fn inherit_target(db: &dyn HirDb, def: DefId, base_name: &str) -> Option<DefId> {
    let contract = db.owner(def)?;
    if !matches!(contract, DefId::Contract(_) | DefId::Interface(_)) {
        return None;
    }
    let name = db.file_def_map(def.file()).defs.get(&def)?.name.clone()?;
    let namespace = DefNamespace::from(&def);

    let base = db
        .bases(contract)
        .iter()
        .skip(1)
        .copied()
        .find(|&base| {
            db.file_def_map(base.file())
                .defs
                .get(&base)
                .and_then(|data| data.name.as_deref())
                == Some(base_name)
        })?;

    let defmap = db.file_def_map(base.file());
    let scope = defmap.defs.get(&base)?.child_scope?;
    let candidates = defmap.scopes[scope].by_name.get(name.as_str())?;
    if candidates.namespace != namespace {
        return None;
    }
    let mut best: Option<(usize, DefId)> = None;
    for &candidate in &candidates.defs {
        if candidate == def {
            continue;
        }
        let score = signature_match(db, def, candidate);
        if best.is_none_or(|(top, _)| score > top) {
            best = Some((score, candidate));
        }
    }
    best.map(|(_, candidate)| candidate)
}

/// `1 +` the count of positionally-equal parameter types, or 0 when arities
/// differ or either side lacks a signature. Same-arity candidates always beat
/// signature-less ones.
fn signature_match(db: &dyn HirDb, def: DefId, candidate: DefId) -> usize {
    let (Some(own), Some(other)) = (signature_of(db, def), signature_of(db, candidate)) else {
        return 0;
    };
    if own.params.len() != other.params.len() {
        return 0;
    }
    1 + own
        .params
        .iter()
        .zip(other.params.iter())
        .filter(|(a, b)| a == b)
        .count()
}

/// Doc comments for a declaration at an exact range (e.g. struct fields, enum variants)
/// Untracked: only used on demand by capabilities like hover
pub fn inline_docs(db: &dyn RootDb, file: File, range: NodeRange) -> Option<String> {
    let root = db.root(file);
    let node = root.named_child_node(range)?;
    let raw = collect_doc_comments(&root, &node)?;
    Some(normalize_doc(&raw))
}

/// Doc comments (`///` or `/** */`) directly preceding a declaration, joined
/// and unnormalized. `//` and `/* */` are plain comments — skipped, never
/// producing docs.
fn collect_doc_comments(root: &AstNode, node: &AstNode) -> Option<String> {
    let mut comments = Vec::new();
    let mut current = node.node().prev_named_sibling();
    while let Some(sibling) = current {
        if sibling.kind_id() != NodeKind::COMMENT {
            break;
        }
        let text = root.text_by_range(sibling.byte_range());
        if text.starts_with("///") || text.starts_with("/**") {
            comments.push(text.to_string());
        }
        current = sibling.prev_named_sibling();
    }

    if comments.is_empty() {
        return None;
    }
    comments.reverse();
    Some(comments.join("\n"))
}

/// Parsed NatSpec — the merge stage between raw comment text and markdown.
/// Entries keep source order; `inherit` carries the `@inheritdoc` base name
/// for `doc_tags` to consume.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct DocTags {
    pub(crate) entries: Vec<DocEntry>,
    inherit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DocEntry {
    Text(String),
    Notice(String),
    Dev(String),
    Title(String),
    Author(String),
    Param { name: String, desc: String },
    Return(String),
    Custom(String),
}

impl DocTags {
    
    pub(crate) fn merge_missing(&mut self, base: &DocTags) {
        let has_notice = self
            .entries
            .iter()
            .any(|e| matches!(e, DocEntry::Notice(_)));
        let has_dev = self.entries.iter().any(|e| matches!(e, DocEntry::Dev(_)));
        let has_title = self
            .entries
            .iter()
            .any(|e| matches!(e, DocEntry::Title(_)));
        let has_author = self
            .entries
            .iter()
            .any(|e| matches!(e, DocEntry::Author(_)));
        let has_return = self
            .entries
            .iter()
            .any(|e| matches!(e, DocEntry::Return(_)));
        let params: FxHashSet<String> = self
            .entries
            .iter()
            .filter_map(|e| match e {
                DocEntry::Param { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        let customs: FxHashSet<String> = self
            .entries
            .iter()
            .filter_map(|e| match e {
                DocEntry::Custom(text) => Some(custom_tag(text).to_string()),
                _ => None,
            })
            .collect();

        self.entries.extend(
            base.entries
                .iter()
                .filter(|entry| match entry {
                    DocEntry::Text(_) => true,
                    DocEntry::Notice(_) => !has_notice,
                    DocEntry::Dev(_) => !has_dev,
                    DocEntry::Title(_) => !has_title,
                    DocEntry::Author(_) => !has_author,
                    DocEntry::Return(_) => !has_return,
                    DocEntry::Param { name, .. } => !params.contains(name.as_str()),
                    DocEntry::Custom(text) => !customs.contains(custom_tag(text)),
                })
                .cloned(),
        );
    }
}

/// The `@custom:<tag>` name — the first word of the tag's text.
fn custom_tag(text: &str) -> &str {
    text.split_whitespace().next().unwrap_or_default()
}

/// Splits raw doc-comment text into tag entries. `@inheritdoc <name>` is
/// captured in `DocTags::inherit` for `doc_tags` to expand; a bare
/// `@inheritdoc` survives as text so the stray tag stays visible.
pub(crate) fn parse_doc(raw: &str) -> DocTags {
    let mut doc = DocTags::default();
    let mut code_block = false;
    for raw_line in raw.lines() {
        let mut trimmed_line = raw_line.trim().trim_matches(['/', '*']);

        if code_block {
            let line = trimmed_line.strip_prefix(' ').unwrap_or(trimmed_line);
            doc.entries.push(DocEntry::Text(line.to_string()));
            if line.starts_with("```") {
                code_block = false;
            }
            continue;
        }

        trimmed_line = trimmed_line.trim();
        if trimmed_line.starts_with("```") {
            code_block = true;
        }
        if let Some(text) = trimmed_line.strip_prefix("@notice") {
            doc.entries.push(DocEntry::Notice(text.to_string()));
        } else if let Some(text) = trimmed_line.strip_prefix("@dev") {
            doc.entries.push(DocEntry::Dev(text.to_string()));
        } else if let Some(text) = trimmed_line.strip_prefix("@param") {
            let mut parts = text.trim().splitn(2, char::is_whitespace);
            doc.entries.push(DocEntry::Param {
                name: parts.next().unwrap_or_default().to_string(),
                desc: parts.next().unwrap_or_default().trim().to_string(),
            });
        } else if let Some(text) = trimmed_line.strip_prefix("@return") {
            doc.entries.push(DocEntry::Return(text.to_string()));
        } else if let Some(text) = trimmed_line.strip_prefix("@title") {
            doc.entries.push(DocEntry::Title(text.to_string()));
        } else if let Some(text) = trimmed_line.strip_prefix("@author") {
            doc.entries.push(DocEntry::Author(text.to_string()));
        } else if let Some(text) = trimmed_line.strip_prefix("@custom:") {
            doc.entries.push(DocEntry::Custom(text.to_string()));
        } else if let Some(text) = trimmed_line.strip_prefix("@inheritdoc") {
            let name = text.trim();
            if name.is_empty() {
                doc.entries.push(DocEntry::Text(trimmed_line.to_string()));
            } else {
                doc.inherit.get_or_insert_with(|| name.to_string());
            }
        } else {
            doc.entries.push(DocEntry::Text(trimmed_line.to_string()));
        }
    }
    doc
}

pub(crate) fn format_doc(doc: &DocTags) -> String {
    doc.entries
        .iter()
        .map(|entry| match entry {
            DocEntry::Text(line) => line.clone(),
            DocEntry::Notice(text) => format!("\n**Notice:**{text}"),
            DocEntry::Dev(text) => format!("\n**Dev note:**{text}"),
            DocEntry::Title(text) => format!("\n**Title:**{text}"),
            DocEntry::Author(text) => format!("\n**Author:**{text}"),
            DocEntry::Param { name, desc } if desc.is_empty() => format!("- **`{name}`**"),
            DocEntry::Param { name, desc } => format!("- **`{name}`**: {desc}"),
            DocEntry::Return(text) => format!("- **Returns**{text}"),
            DocEntry::Custom(text) => format!("\n**Custom:** {text}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn normalize_doc(raw: &str) -> String {
    format_doc(&parse_doc(raw))
}

