use std::cell::RefCell;

use camino::Utf8Path;
use rustc_hash::FxHashSet;
use tree_sitter::Parser;
use triomphe::Arc;

use super::db::{Db, File, FileId, Packages, SourceRootId};
use crate::ast::kinds::FieldKind;
use crate::ast::{Ast, AstIdMap, AstNode, NodeRange};
use crate::hir::analysis::Analysis;
use crate::ir::def_map::{Collector, DefId, DefMap};
use crate::ir::import_map::{ImportMap, Imports, SourceUnit, source_unit_name};
use crate::ir::item_tree::{ItemId, ItemTree, Lowerer};
use crate::workspace::Package;

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new({
        let mut parser = Parser::new();
        parser.set_language(&tree_sitter_solidity::LANGUAGE.into()).expect("parser set language");
        parser
    });
}

#[salsa::db]
pub trait RootDb: salsa::Database {
    fn text(&self, file: FileId) -> Arc<str>;
    fn file_source_root(&self, file: FileId) -> SourceRootId;
    fn source_roots(&self) -> &[SourceRootId];
    fn package_config(&self, root_id: SourceRootId) -> &Package;
    fn ast(&self, file: File) -> Arc<Ast>;
    fn root(&self, file: File) -> AstNode;
    fn ast_id_map(&self, file: File) -> Arc<AstIdMap>;
    fn item_tree(&self, file: File) -> &ItemTree;
    fn root_def_map_analysis(&self, root: SourceRootId) -> Analysis<DefMap>;
    fn root_def_map(&self, root: SourceRootId) -> Arc<DefMap>;
    fn file_def_map(&self, file: File) -> Arc<DefMap>;
    fn resolve_to_file(&self, from: FileId, import_path: &str) -> Option<FileId>;
    fn is_ignored_path(&self, path: &Utf8Path) -> bool;
    fn imports(&self, file: File) -> &Imports;
    fn imports_closure(&self, file: File) -> &[SourceUnit];
    fn dependents_of(&self, anchor: File) -> &FxHashSet<File>;
}

#[salsa::db]
impl RootDb for Db {
    fn text(&self, file: FileId) -> Arc<str> {
        text(self, file)
    }

    fn file_source_root(&self, file: FileId) -> SourceRootId {
        self.file_source_root(file)
    }

    fn source_roots(&self) -> &[SourceRootId] {
        self.source_roots()
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
        self.root_def_map(file.root(self))
    }

    fn resolve_to_file(&self, from: FileId, rel_path: &str) -> Option<FileId> {
        self.resolve_to_file(from, rel_path)
    }

    fn is_ignored_path(&self, path: &Utf8Path) -> bool {
        self.is_ignored_path(path)
    }

    fn imports(&self, file: File) -> &Imports {
        imports(self, file)
    }

    fn imports_closure(&self, file: File) -> &[SourceUnit] {
        imports_closure(self, file)
    }

    fn dependents_of(&self, anchor: File) -> &FxHashSet<File> {
        dependents_of(self, anchor)
    }
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

#[salsa::tracked(cycle_fn = root_def_map_analysis_recover, cycle_initial = root_def_map_analysis_initial)]
pub fn root_def_map_analysis(db: &dyn RootDb, root: SourceRootId) -> Analysis<DefMap> {
    Collector::collect_defmap(db, root)
}

/// The bottom of the domain: a root that has bound no names yet.
fn root_def_map_analysis_initial(_db: &dyn RootDb, _id: salsa::Id, root: SourceRootId) -> Analysis<DefMap> {
    Analysis::new(DefMap::empty(root), Arc::from(Vec::new()))
}

/// Keep what the round computed; salsa re-runs until two rounds agree, which
/// it decides by comparing the maps and their diagnostics.
fn root_def_map_analysis_recover(
    _db: &dyn RootDb,
    _cycle: &salsa::Cycle,
    _last_provisional: &Analysis<DefMap>,
    value: Analysis<DefMap>,
    _root: SourceRootId,
) -> Analysis<DefMap> {
    value
}

/// The imports `file` declares, resolved.
/// i.e. files that 'file' depend on.
#[salsa::tracked(returns(ref))]
pub fn imports(db: &dyn RootDb, file: File) -> Imports {
    let tree = db.item_tree(file);
    let mut imports = Imports::default();
    for &item in tree.top_level.iter() {
        let ItemId::Import(id) = item else { continue };
        let path = &tree[id].path;
        match db.resolve_to_file(file, path.as_str()) {
            // Two directives that resolve to one file are one edge.
            Some(target) if !imports.resolved.contains(&target) => imports.resolved.push(target),
            Some(_) => {}
            None => imports.unresolved.push(path.as_str().into()),
        }
    }
    imports
}

/// The inversion of [`imports`] for one root: for each file, its importers.
#[salsa::tracked(returns(ref))]
pub fn import_map(db: &dyn RootDb, root: SourceRootId) -> ImportMap {
    let mut map = ImportMap::default();
    for &file in root.files(db).iter() {
        for &target in &imports(db, file).resolved {
            map.insert(target, file);
        }
    }
    map
}

/// Every file that transitively imports `anchor`.
///
/// One walk across the per-root inversions. A workspace anchor stays in
/// workspace roots: a dependency cannot depend on a file in a non-dep root. A
/// dependency anchor includes dependency roots, because dependency files can
/// import one another.
#[salsa::tracked(returns(ref))]
pub fn dependents_of(db: &dyn RootDb, anchor: File) -> FxHashSet<File> {
    let include_dependencies = anchor.root(db).is_dependency(db);
    let maps: Vec<&ImportMap> = db
        .source_roots()
        .iter()
        .copied()
        .filter(|&root| include_dependencies || !root.is_dependency(db))
        .map(|root| import_map(db, root))
        .collect();

    let mut dependents = FxHashSet::default();
    let mut pending = vec![anchor];
    while let Some(file) = pending.pop() {
        for map in &maps {
            for &importer in map.of(file) {
                if dependents.insert(importer) {
                    pending.push(importer);
                }
            }
        }
    }
    dependents
}

/// `entry` and everything it reaches, named relative to the entry's package
/// root — the source map solc is handed.
#[salsa::tracked(returns(ref))]
pub fn imports_closure(db: &dyn RootDb, entry: File) -> Vec<SourceUnit> {
    let base = db.package_config(entry.root(db)).root.clone();
    let mut closure = vec![SourceUnit {
        name: source_unit_name(&base, entry.path(db)),
        file: entry,
    }];
    let mut seen = FxHashSet::from_iter([entry]);
    let mut pending = vec![entry];
    while let Some(file) = pending.pop() {
        for &target in &imports(db, file).resolved {
            if seen.insert(target) {
                closure.push(SourceUnit {
                    name: source_unit_name(&base, target.path(db)),
                    file: target,
                });
                pending.push(target);
            }
        }
    }
    closure
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
