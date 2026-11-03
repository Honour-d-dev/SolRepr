use std::collections::hash_map::Entry;
use std::mem;
use std::ops::Index;
use std::ops::IndexMut;

use la_arena::{Arena, Idx};
use rustc_hash::FxHashMap;
use rustc_hash::FxHashSet;
use smallvec::SmallVec;
use smol_str::SmolStr;

use crate::ast::ImportId;
use crate::ast::ImportItem;
use crate::ast::NodeRange;
use crate::ast::UdvtId;
use crate::ast::UsingId;
use crate::ast::{
    ContractId, EnumId, ErasedAstId, ErrorId, EventId, FunctionId, ImportKind, InterfaceId,
    LibraryId, ModifierId, StructId, VarId,
};
use crate::hir::analysis::Analysis;
use crate::hir::semantic_errors::{DiagnosticSink, SemanticDiagnostic};
use crate::ir::item_tree::{ItemId, ItemTree};
use crate::salsa::root_db::def_range;
use crate::salsa::{FileId, RootDb, SourceRootId};

type Name = SmolStr;
pub type ScopeId = Idx<Scope>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DefId {
    File(FileId),
    Import(ImportId),
    Udvt(UdvtId),
    Using(UsingId),
    Contract(ContractId),
    Interface(InterfaceId),
    Library(LibraryId),
    Function(FunctionId),
    Modifier(ModifierId),
    Struct(StructId),
    Event(EventId),
    Enum(EnumId),
    Error(ErrorId),
    Var(VarId),
}

impl DefId {
    pub fn file_and_id(&self) -> (FileId, Option<ErasedAstId>) {
        match self {
            DefId::File(f) => (*f, None),
            DefId::Import(i) => (i.file, Some(i.id.erase())),
            DefId::Udvt(u) => (u.file, Some(u.id.erase())),
            DefId::Using(u) => (u.file, Some(u.id.erase())),
            DefId::Contract(c) => (c.file, Some(c.id.erase())),
            DefId::Interface(i) => (i.file, Some(i.id.erase())),
            DefId::Library(l) => (l.file, Some(l.id.erase())),
            DefId::Function(f) => (f.file, Some(f.id.erase())),
            DefId::Modifier(m) => (m.file, Some(m.id.erase())),
            DefId::Struct(s) => (s.file, Some(s.id.erase())),
            DefId::Event(e) => (e.file, Some(e.id.erase())),
            DefId::Enum(e) => (e.file, Some(e.id.erase())),
            DefId::Error(e) => (e.file, Some(e.id.erase())),
            DefId::Var(v) => (v.file, Some(v.id.erase())),
        }
    }

    pub fn file(&self) -> FileId {
        self.file_and_id().0
    }

    pub fn id(&self) -> Option<ErasedAstId> {
        self.file_and_id().1
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Scope {
    pub owner: DefId,
    pub parent: Option<ScopeId>,
    pub by_name: FxHashMap<Name, ScopeData>,
    pub usings: Option<Vec<DefId>>,
}

#[derive(PartialEq, Eq, Clone)]
pub struct ScopeData {
    pub namespace: DefNamespace,
    pub defs: SmallVec<[DefId; 1]>, //most names wont be overloaded
}

/// Overloads can only occur per namespace in a scope and some namespaces dont support overloads within same scope. e.g types and variables
#[derive(PartialEq, Eq, Clone, Copy, Hash)]
pub enum DefNamespace {
    Types, //File, Contract, Interface, Library, Struct, Enum
    Functions,
    Errors,
    Events,
    Modifiers,
    Variables,
}

impl DefNamespace {
    pub fn is_overloadable(&self) -> bool {
        !matches!(
            self,
            DefNamespace::Types | DefNamespace::Variables | DefNamespace::Modifiers
        )
    }

    pub fn from(def: &DefId) -> DefNamespace {
        match def {
            DefId::File(_)
            | DefId::Import(_)
            | DefId::Contract(_)
            | DefId::Interface(_)
            | DefId::Library(_)
            | DefId::Struct(_)
            | DefId::Enum(_)
            | DefId::Udvt(_)
            | DefId::Using(_) => DefNamespace::Types,
            DefId::Function(_) => DefNamespace::Functions,
            DefId::Modifier(_) => DefNamespace::Modifiers,
            DefId::Event(_) => DefNamespace::Events,
            DefId::Error(_) => DefNamespace::Errors,
            DefId::Var(_) => DefNamespace::Variables,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct DefData {
    pub name: Option<SmolStr>,
    pub scope: ScopeId,
    pub child_scope: Option<ScopeId>,
}

#[derive(Default, Clone, PartialEq, Eq)]
pub struct UnresolvedImports {
    pub imports: Vec<ImportId>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScopeEntry {
    pub root: SourceRootId,
    pub scope: ScopeId,
}

#[derive(Clone, PartialEq, Eq)]
pub struct FileData {
    pub id: DefId,
    pub scope: ScopeId,
    pub imported_scopes: Vec<ScopeEntry>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct DefMap {
    pub root: SourceRootId,
    pub files: FxHashMap<FileId, FileData>,
    pub scopes: Arena<Scope>,
    pub defs: FxHashMap<DefId, DefData>,
    pub unresolved: FxHashMap<FileId, UnresolvedImports>,
}

pub struct FileCollector<'db, 'collector> {
    file: FileId,
    item_tree: &'db ItemTree,
    collector: &'collector mut Collector<'db>,
}

impl<'db, 'collector> FileCollector<'db, 'collector> {
    fn collect_top(&mut self) {
        let file_scope = self.collect_file();
        let top_level = &self.item_tree.top_level;

        for &item_id in top_level.iter() {
            match item_id {
                ItemId::Import(id) => {
                    // Import defs are not collected into scopes.
                    let def_data = DefData {
                        name: None,
                        scope: file_scope,
                        child_scope: None,
                    };
                    let id = ImportId {
                        file: self.file,
                        id,
                    };
                    self.collector.defs.insert(DefId::Import(id), def_data);
                    self.collector
                        .unresolved_imports
                        .entry(self.file)
                        .or_default()
                        .imports
                        .push(id);
                }
                ItemId::Using(id) => {
                    let data = DefData {
                        name: None,
                        scope: file_scope,
                        child_scope: None,
                    };
                    let id = DefId::Using(UsingId {
                        file: self.file,
                        id,
                    });
                    self.collector.defs.insert(id, data);
                    self.collector.scopes[file_scope]
                        .usings
                        .get_or_insert_with(Vec::new)
                        .push(id);
                }
                ItemId::Contract(id) => {
                    let c = &self.item_tree[id];
                    let id = DefId::Contract(ContractId {
                        file: self.file,
                        id,
                    });
                    self.collect_container(id, file_scope, c.name.clone(), &c.members);
                }
                ItemId::Interface(id) => {
                    let i = &self.item_tree[id];
                    let id = DefId::Interface(InterfaceId {
                        file: self.file,
                        id,
                    });
                    self.collect_container(id, file_scope, i.name.clone(), &i.members);
                }
                ItemId::Library(id) => {
                    let l = &self.item_tree[id];
                    let id = DefId::Library(LibraryId {
                        file: self.file,
                        id,
                    });
                    self.collect_container(id, file_scope, l.name.clone(), &l.members);
                }
                ItemId::Udvt(id) => {
                    let u = &self.item_tree[id];
                    let id = DefId::Udvt(UdvtId {
                        file: self.file,
                        id,
                    });
                    self.collect_def(id, file_scope, None, u.name.clone().unwrap_or_default());
                }
                ItemId::Struct(id) => {
                    let s = &self.item_tree[id];
                    let id = DefId::Struct(StructId {
                        file: self.file,
                        id,
                    });
                    self.collect_def(id, file_scope, None, s.name.clone());
                }
                ItemId::Enum(id) => {
                    let e = &self.item_tree[id];
                    let id = DefId::Enum(EnumId {
                        file: self.file,
                        id,
                    });
                    self.collect_def(id, file_scope, None, e.name.clone());
                }
                ItemId::Function(id) => {
                    let f = &self.item_tree[id];
                    let id = DefId::Function(FunctionId {
                        file: self.file,
                        id,
                    });
                    self.collect_def(id, file_scope, None, f.name.clone());
                }
                ItemId::Event(id) => {
                    let e = &self.item_tree[id];
                    let id = DefId::Event(EventId {
                        file: self.file,
                        id,
                    });
                    self.collect_def(id, file_scope, None, e.name.clone());
                }
                ItemId::Error(id) => {
                    let e = &self.item_tree[id];
                    let id = DefId::Error(ErrorId {
                        file: self.file,
                        id,
                    });
                    self.collect_def(id, file_scope, None, e.name.clone());
                }
                ItemId::Modifier(id) => {
                    let m = &self.item_tree[id];
                    let id = DefId::Modifier(ModifierId {
                        file: self.file,
                        id,
                    });
                    self.collect_def(id, file_scope, None, m.name.clone());
                }
                ItemId::Var(id) => {
                    let v = &self.item_tree[id];
                    let id = DefId::Var(VarId {
                        file: self.file,
                        id,
                    });
                    self.collect_def(id, file_scope, None, v.name.clone());
                }
            }
        }
    }

    fn collect_file(&mut self) -> ScopeId {
        let id = DefId::File(self.file);
        let scope_id = self.collector.scopes.alloc(Scope {
            owner: id,
            parent: None,
            by_name: FxHashMap::default(),
            usings: Default::default(),
        });

        self.collector.files.insert(
            self.file,
            FileData {
                id,
                scope: scope_id,
                imported_scopes: Vec::new(),
            },
        );

        //Insert files as defs as well. so we can import as namespace
        self.collector.defs.insert(
            id,
            DefData {
                name: None,
                scope: scope_id,
                child_scope: Some(scope_id),
            },
        );
        scope_id
    }

    fn collect_container(
        &mut self,
        id: DefId,
        file_scope: ScopeId,
        name: Name,
        members: &[ItemId],
    ) {
        let scope = Scope {
            owner: id,
            parent: Some(file_scope),
            by_name: FxHashMap::default(),
            usings: Default::default(),
        };

        let container_scope = self.collector.scopes.alloc(scope);

        self.collect_def(id, file_scope, Some(container_scope), name);
        self.collect_members(members, container_scope);
    }

    fn collect_members(&mut self, members: &[ItemId], parent_scope: ScopeId) {
        for &member in members {
            let (id, name) = match member {
                ItemId::Struct(id) => (
                    DefId::Struct(StructId {
                        file: self.file,
                        id,
                    }),
                    self.item_tree[id].name.clone(),
                ),
                ItemId::Enum(id) => (
                    DefId::Enum(EnumId {
                        file: self.file,
                        id,
                    }),
                    self.item_tree[id].name.clone(),
                ),
                ItemId::Function(id) => (
                    DefId::Function(FunctionId {
                        file: self.file,
                        id,
                    }),
                    self.item_tree[id].name.clone(),
                ),
                ItemId::Event(id) => (
                    DefId::Event(EventId {
                        file: self.file,
                        id,
                    }),
                    self.item_tree[id].name.clone(),
                ),
                ItemId::Error(id) => (
                    DefId::Error(ErrorId {
                        file: self.file,
                        id,
                    }),
                    self.item_tree[id].name.clone(),
                ),
                ItemId::Modifier(id) => (
                    DefId::Modifier(ModifierId {
                        file: self.file,
                        id,
                    }),
                    self.item_tree[id].name.clone(),
                ),
                ItemId::Var(id) => (
                    DefId::Var(VarId {
                        file: self.file,
                        id,
                    }),
                    self.item_tree[id].name.clone(),
                ),
                ItemId::Udvt(id) => (
                    DefId::Udvt(UdvtId {
                        file: self.file,
                        id,
                    }),
                    self.item_tree[id].name.clone().unwrap_or_default(),
                ),
                ItemId::Using(id) => {
                    let data = DefData {
                        name: None,
                        scope: parent_scope,
                        child_scope: None,
                    };
                    let id = DefId::Using(UsingId {
                        file: self.file,
                        id,
                    });
                    self.collector.defs.insert(id, data);
                    self.collector.scopes[parent_scope]
                        .usings
                        .get_or_insert_with(Vec::new)
                        .push(id);
                    continue;
                }
                _ => continue,
            };

            self.collect_def(id, parent_scope, None, name);
        }
    }

    fn collect_def(
        &mut self,
        id: DefId,
        scope_id: ScopeId,
        sub_scope: Option<ScopeId>,
        name: Name,
    ) {
        let scope = self.collector.scopes.index_mut(scope_id);
        let ns = DefNamespace::from(&id);
        let mut previous = None;
        match scope.by_name.entry(name.clone()) {
            Entry::Occupied(mut entry) => {
                let e = entry.get_mut();
                if e.namespace == ns && ns.is_overloadable() {
                    e.defs.push(id);
                } else {
                    previous = Some(e.defs[0]);
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(ScopeData {
                    namespace: ns,
                    defs: SmallVec::from([id]),
                });
            }
        }
        if let Some(previous) = previous {
            self.collector
                .report_redeclaration(id, name.clone(), previous);
        }
        let def_data = DefData {
            name: Some(name),
            scope: scope_id,
            child_scope: sub_scope,
        };
        self.collector.defs.insert(id, def_data);
    }
}

/// A named-import symbol that was not found in its dependency during the DFS. The dependency's
/// own named imports may still bind it, so it is re-checked once every import has resolved.
struct PendingSymbol {
    import: ImportId,
    dep: FileId,
    dep_root: SourceRootId,
    symbol: ImportItem,
}

pub struct Collector<'db> {
    db: &'db dyn RootDb,
    root: SourceRootId,
    pub files: FxHashMap<FileId, FileData>,
    pub scopes: Arena<Scope>,
    pub defs: FxHashMap<DefId, DefData>,
    pub unresolved_imports: FxHashMap<FileId, UnresolvedImports>,
    sink: DiagnosticSink,
    pending_symbols: Vec<PendingSymbol>,
}

impl<'db> Collector<'db> {
    pub fn collect_defmap(
        db: &'db dyn RootDb,
        source_root_id: SourceRootId,
    ) -> Analysis<DefMap> {
        let files = source_root_id.files(db).as_ref();

        let mut collector = Collector {
            db,
            root: source_root_id,
            files: FxHashMap::default(),
            scopes: Arena::default(),
            defs: FxHashMap::default(),
            unresolved_imports: FxHashMap::default(),
            sink: DiagnosticSink::default(),
            pending_symbols: Vec::new(),
        };

        for file in files.iter() {
            FileCollector {
                file: *file,
                item_tree: db.item_tree(*file),
                collector: &mut collector,
            }
            .collect_top();
        }

        collector.resolve_imports();
        collector.bind_pending_symbols();

        let diagnostics = mem::take(&mut collector.sink).finish();
        Analysis::new(
            DefMap {
                root: collector.root,
                files: collector.files,
                scopes: collector.scopes,
                defs: collector.defs,
                unresolved: collector.unresolved_imports,
            },
            diagnostics,
        )
    }

    fn report_redeclaration(&mut self, rejected: DefId, name: Name, previous: DefId) {
        let range = def_range(self.db, rejected).unwrap_or(NodeRange { start: 0, end: 0 });
        self.sink.push(SemanticDiagnostic::Redeclaration {
            rejected,
            range,
            name,
            previous,
        });
    }

    /// Resolving imports is tricky because a global import brings its own imported global scope
    ///
    /// i.e. A imports B but B imports C, so A auto imports C via B (i.e has C's global scope as well)
    ///
    /// But Solidity also supports cyclic imports, which mean C can also import A (inheriting A's global scope too) typical chicken<->hen problem
    ///
    /// Now we have an import loop. where all files(A,B,C) essentially have/share the same global scope.
    fn resolve_imports(&mut self) {
        while let Some(&file_id) = self.unresolved_imports.keys().next() {
            let mut seen = FxHashSet::default();
            seen.insert(file_id);
            // Imports still in a cycle after the chain unwinds are dropped: a legal cycle binds
            // through its other edges, and an unresolvable path was already reported.
            if self.resolve(file_id, &mut seen) != ImportResolver::Finished {
                self.unresolved_imports.remove(&file_id);
            }
        }
    }

    fn resolve(&mut self, file_id: FileId, chain: &mut FxHashSet<FileId>) -> ImportResolver {
        let db = self.db;
        let mut unresolved = match self.unresolved_imports.get_mut(&file_id) {
            Some(u) => mem::take(u),
            None => UnresolvedImports::default(),
        };
        let mut resolved = FxHashSet::default();
        let mut resolver = ImportResolver::Finished;

        for &import_id in unresolved.imports.iter() {
            let import = &db.item_tree(import_id.file)[import_id.id];
            let Some(dep) = self.db.resolve_to_file(file_id, import.path.as_str()) else {
                self.sink.push(SemanticDiagnostic::UnresolvedImport {
                    import: import_id,
                    range: def_range(db, DefId::Import(import_id))
                        .unwrap_or(NodeRange { start: 0, end: 0 }),
                    path: import.path.clone(),
                });
                continue;
            };
            let other_root = self.db.file_source_root(dep);

            if self.root == other_root {
                if self.unresolved_imports.contains_key(&dep) {
                    if chain.insert(dep) {
                        match self.resolve(dep, chain) {
                            ImportResolver::Finished => {
                                resolved.insert(import_id);
                            }
                            c @ ImportResolver::Cycle(_) => {
                                resolver.merge(c);
                            }
                        }
                        self.extend_scope(import_id, dep, self.root, &import.import_kind);
                    } else {
                        resolver.merge(ImportResolver::Cycle(vec![dep]));
                    }
                } else {
                    self.extend_scope(import_id, dep, self.root, &import.import_kind);
                    resolved.insert(import_id);
                }
            } else {
                self.extend_scope(import_id, dep, other_root, &import.import_kind);
                resolved.insert(import_id);
            }
        }

        match resolver.resolve(file_id) {
            f @ ImportResolver::Finished => {
                // A cycle is "seen in this chain", not "seen before"; a resolved
                // path leaves the chain as it unwinds.
                self.unresolved_imports.remove(&file_id);
                chain.remove(&file_id);
                f
            }
            c @ ImportResolver::Cycle(_) => {
                unresolved.imports.retain(|i| !resolved.contains(i));
                self.unresolved_imports.insert(file_id, unresolved);
                c
            }
        }
    }

    fn extend_scope(
        &mut self,
        import: ImportId,
        dep: FileId,
        dep_root: SourceRootId,
        import_kind: &ImportKind,
    ) {
        let file = import.file;
        match import_kind {
            ImportKind::Full => {
                let dep_data = self.file_data(dep_root, dep);
                let file_data = self.files.get_mut(&file).unwrap();
                push_unique_scope(
                    &mut file_data.imported_scopes,
                    ScopeEntry {
                        root: dep_root,
                        scope: dep_data.scope,
                    },
                );
                for scope in dep_data.imported_scopes {
                    push_unique_scope(&mut file_data.imported_scopes, scope);
                }
            }
            ImportKind::Named { symbols } => {
                for symbol in symbols {
                    if !self.bind_named_symbol(import, dep, dep_root, symbol) {
                        self.pending_symbols.push(PendingSymbol {
                            import,
                            dep,
                            dep_root,
                            symbol: symbol.clone(),
                        });
                    }
                }
            }
            ImportKind::Namespace { alias } => {
                let dep_def_id = self.file_data(dep_root, dep).id;
                let file_scope = self.files.get(&file).unwrap().scope;
                let mut previous = None;
                match self
                    .scopes
                    .index_mut(file_scope)
                    .by_name
                    .entry(alias.clone())
                {
                    Entry::Vacant(v) => {
                        v.insert(ScopeData {
                            namespace: DefNamespace::Types,
                            defs: [dep_def_id].into(),
                        });
                    }
                    // Already bound to this file by an earlier attempt of the same directive.
                    Entry::Occupied(e) if e.get().defs.contains(&dep_def_id) => {}
                    Entry::Occupied(e) => previous = Some(e.get().defs[0]),
                }
                if let Some(previous) = previous {
                    self.report_redeclaration(DefId::Import(import), alias.clone(), previous);
                }
            }
        }
        self.extend_global_usings(file, dep, dep_root);
    }

    /// Binds one named-import symbol into its importing file's scope. `false` means the symbol
    /// is not visible in the dependency yet and must be retried.
    fn bind_named_symbol(
        &mut self,
        import: ImportId,
        dep: FileId,
        dep_root: SourceRootId,
        symbol: &ImportItem,
    ) -> bool {
        let Some(ids) = self.find_name_in_file(dep_root, dep, &symbol.name) else {
            return false;
        };
        let file_scope = self.files.get(&import.file).unwrap().scope;
        let name = symbol.alias.clone().unwrap_or(symbol.name.clone());
        let mut previous = None;
        match self
            .scopes
            .index_mut(file_scope)
            .by_name
            .entry(name.clone())
        {
            Entry::Vacant(v) => {
                v.insert(ids);
            }
            Entry::Occupied(mut entry) => {
                let e = entry.get_mut();
                let same_namespace = e.namespace == ids.namespace;
                if same_namespace && e.namespace.is_overloadable() {
                    extend_unique_defs(&mut e.defs, ids.defs);
                } else if !(same_namespace && ids.defs.iter().all(|d| e.defs.contains(d))) {
                    previous = Some(e.defs[0]);
                }
            }
        }
        if let Some(previous) = previous {
            self.report_redeclaration(DefId::Import(import), name, previous);
        }
        true
    }

    fn bind_pending_symbols(&mut self) {
        let db = self.db;
        for pending in mem::take(&mut self.pending_symbols) {
            if !self.bind_named_symbol(
                pending.import,
                pending.dep,
                pending.dep_root,
                &pending.symbol,
            ) {
                self.sink.push(SemanticDiagnostic::ImportSymbolNotFound {
                    import: pending.import,
                    range: def_range(db, DefId::Import(pending.import))
                        .unwrap_or(NodeRange { start: 0, end: 0 }),
                    name: pending.symbol.name.clone(),
                    path: db.item_tree(pending.import.file)[pending.import.id]
                        .path
                        .clone(),
                });
            }
        }
    }

    fn extend_global_usings(&mut self, file: FileId, dep: FileId, dep_root: SourceRootId) {
        let dep_data = self.file_data(dep_root, dep);
        let file_scope = self.files.get(&file).unwrap().scope;
        // DFS resolution guarantees all globals (up to this point) are already accumulated in dep scope. no need to check imported scopes
        let to_add: Vec<DefId> = {
            let other_defmap;
            let usings = if self.root == dep_root {
                self.scopes[dep_data.scope].usings.as_deref()
            } else {
                other_defmap = self.db.root_def_map(dep_root);
                other_defmap.scopes[dep_data.scope].usings.as_deref()
            };
            usings
                .into_iter()
                .flatten()
                .filter(|&using_def| {
                    if let DefId::Using(using_id) = using_def {
                        self.db.item_tree(using_id.file)[using_id.id].is_global
                    } else {
                        false
                    }
                })
                .copied()
                .collect()
        };
        for using_def in to_add {
            let file_usings = self.scopes[file_scope].usings.get_or_insert_with(Vec::new);
            if !file_usings.contains(&using_def) {
                file_usings.push(using_def);
            }
        }
    }

    fn file_data(&self, root: SourceRootId, file: FileId) -> FileData {
        if self.root == root {
            self.files.get(&file).unwrap().clone()
        } else {
            self.db.root_def_map(root).files.get(&file).unwrap().clone()
        }
    }

    fn find_name_in_file(
        &self,
        root: SourceRootId,
        file: FileId,
        name: &Name,
    ) -> Option<ScopeData> {
        let file_data = self.file_data(root, file);
        let local_scope = ScopeEntry {
            root,
            scope: file_data.scope,
        };
        self.find_name_in_scope(local_scope, name).or_else(|| {
            file_data
                .imported_scopes
                .iter()
                .find_map(|scope| self.find_name_in_scope(*scope, name))
        })
    }

    fn find_name_in_scope(&self, scope: ScopeEntry, name: &Name) -> Option<ScopeData> {
        if self.root == scope.root {
            self.scopes.index(scope.scope).by_name.get(name).cloned()
        } else {
            self.db
                .root_def_map(scope.root)
                .scopes
                .index(scope.scope)
                .by_name
                .get(name)
                .cloned()
        }
    }
}

fn push_unique_scope(scopes: &mut Vec<ScopeEntry>, scope: ScopeEntry) {
    if !scopes.contains(&scope) {
        scopes.push(scope);
    }
}

fn push_unique_def(defs: &mut SmallVec<[DefId; 1]>, def: DefId) {
    if !defs.contains(&def) {
        defs.push(def);
    }
}

fn extend_unique_defs(defs: &mut SmallVec<[DefId; 1]>, new_defs: impl IntoIterator<Item = DefId>) {
    for def in new_defs {
        push_unique_def(defs, def);
    }
}

#[derive(Default, Clone, PartialEq, Eq, Debug)]
enum ImportResolver {
    #[default]
    Finished,
    Cycle(Vec<FileId>),
}

impl ImportResolver {
    pub fn merge(&mut self, other: ImportResolver) {
        match (self, other) {
            (_, ImportResolver::Finished) => (),
            (this @ ImportResolver::Finished, other @ ImportResolver::Cycle(_)) => *this = other,
            (ImportResolver::Cycle(a), ImportResolver::Cycle(b)) => a.extend(b),
        }
    }

    pub fn resolve(self, file_id: FileId) -> ImportResolver {
        match self {
            ImportResolver::Cycle(mut cycle) => {
                cycle.retain(|&f| f != file_id);
                if cycle.is_empty() {
                    ImportResolver::Finished
                } else {
                    ImportResolver::Cycle(cycle)
                }
            }
            ImportResolver::Finished => self,
        }
    }
}

