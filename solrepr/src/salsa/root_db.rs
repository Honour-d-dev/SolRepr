use std::cell::RefCell;

use camino::Utf8Path;
use tree_sitter::Parser;
use triomphe::Arc;

use super::db::{File, FileId, FilesRevision, Packages, Db, SourceRootId};
use crate::ast::kinds::FieldKind;
use crate::ast::{Ast, AstIdMap, AstNode, NodeRange};
use crate::hir::analysis::Analysis;
use crate::ir::def_map::{Collector, DefId, DefMap};
use crate::ir::item_tree::{ItemTree, Lowerer};
use crate::workspace::Package;

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new({
        let mut parser = Parser::new();
        parser.set_language(&tree_sitter_solidity::LANGUAGE.into()).expect("parser set language");
        parser
    });
}

//caching this so we only do it once per revision. but does this means we store 2 copies of all files?
#[salsa::tracked] //manage lru
pub fn text(db: &dyn salsa::Database, file: File) -> Arc<str> {
    file.text(db).to_string().into()
}

#[salsa::tracked(returns(ref))]
pub fn package_config(db: &dyn salsa::Database, root: SourceRootId) -> Package {
    let package_id = root.package_id(db);
    Packages::get(db)
        .packages(db)
        .iter()
        .find(|package| package.root == package_id.0)
        .cloned()
        .expect("a source root names a package that is not in the workspace")
}

#[salsa::tracked]
pub fn parse(db: &dyn RootDb, file: File) -> Arc<Ast> {
    let source = text(db, file);
    let tree = PARSER
        .with(|parser| parser.borrow_mut().parse(source.as_ref(), None))
        .expect("failed to parse Solidity source");
    Arc::new(Ast::new(tree, source))
}

#[salsa::tracked]
pub fn ast_id_map(db: &dyn RootDb, file: File) -> Arc<AstIdMap> {
    let ast = parse(db, file);
    Arc::new(AstIdMap::new(&ast.root()))
}

#[salsa::tracked(returns(ref))]
pub fn item_tree(db: &dyn RootDb, file: File) -> ItemTree {
    Lowerer::lower(db, file)
}

#[salsa::tracked]
pub fn root_def_map_analysis(db: &dyn RootDb, root: SourceRootId) -> Analysis<DefMap> {
    Collector::collect_defmap(db, root)
}

#[salsa::db]
pub trait RootDb: salsa::Database {
    fn text(&self, file: FileId) -> Arc<str>;
    fn file_source_root(&self, file: FileId) -> SourceRootId;
    fn package_config(&self, root_id: SourceRootId) -> &Package;

    fn ast(&self, file: File) -> Arc<Ast>;
    fn root(&self, file: File) -> AstNode;
    fn ast_id_map(&self, file: File) -> Arc<AstIdMap>;
    fn item_tree(&self, file: File) -> &ItemTree;
    fn root_def_map_analysis(&self, root: SourceRootId) -> Analysis<DefMap>;
    fn root_def_map(&self, root: SourceRootId) -> Arc<DefMap>;
    fn file_def_map(&self, file: File) -> Arc<DefMap>;
    fn resolve_to_file(&self, from: FileId, import_path: &str) -> Option<FileId>;
    /// `path` sits under a deduplicated library root — outside the
    /// compilation unit.
    fn is_ignored_path(&self, path: &Utf8Path) -> bool;
}

/// A def's declaration node and its file's root — the walk behind
/// `def_range`. `None` for defs with no AST node.
fn def_node(db: &dyn RootDb, def: DefId) -> Option<(AstNode, AstNode)> {
    let (file, ast_id) = def.file_and_id();
    let ast_id = ast_id?;
    let root = db.root(file);
    let node = db.ast_id_map(file).get_node(&root, ast_id)?.clone();
    Some((root, node))
}

/// Where a def's diagnostics anchor: its written name's range when it has one,
/// else the whole declaration node. Resolved from the AST at report time, so
/// declaration data carries no ranges.
pub fn def_range(db: &dyn RootDb, def: DefId) -> Option<NodeRange> {
    let (_, node) = def_node(db, def)?;
    let name = node.node().child_by_field_id(FieldKind::NAME.into());
    Some(NodeRange::from(&name.unwrap_or(node.node())))
}

#[salsa::db]
impl RootDb for Db {
    fn text(&self, file: FileId) -> Arc<str> {
        text(self, file)
    }

    fn file_source_root(&self, file: FileId) -> SourceRootId {
        FilesRevision::get(self).revision(self);
        self.file_source_root(file).expect("file has no root")
    }

    fn package_config(&self, root: SourceRootId) -> &Package {
        package_config(self, root)
    }

    fn ast(&self, file: File) -> Arc<Ast> {
        parse(self, file)
    }

    fn root(&self, file: File) -> AstNode {
        self.ast(file).root()
    }

    fn ast_id_map(&self, file: File) -> Arc<AstIdMap> {
        ast_id_map(self, file)
    }

    fn item_tree(&self, file: File) -> &ItemTree {
        item_tree(self, file)
    }

    fn root_def_map_analysis(&self, root: SourceRootId) -> Analysis<DefMap> {
        root_def_map_analysis(self, root)
    }

    fn root_def_map(&self, root: SourceRootId) -> Arc<DefMap> {
        self.root_def_map_analysis(root).data
    }

    fn file_def_map(&self, file: File) -> Arc<DefMap> {
        let root = self.file_source_root(file).unwrap();
        self.root_def_map(root)
    }

    fn resolve_to_file(&self, from: FileId, rel_path: &str) -> Option<FileId> {
        FilesRevision::get(self).revision(self);
        self.resolve_to_file(from, rel_path)
    }

    fn is_ignored_path(&self, path: &Utf8Path) -> bool {
        self.is_ignored_path(path)
    }
}
