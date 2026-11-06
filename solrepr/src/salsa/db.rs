use std::panic::AssertUnwindSafe;
use triomphe::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use lsp_types::Diagnostic as LspDiagnostic;
use lsp_types::TextDocumentContentChangeEvent;
use ropey::Rope;
use salsa::{self, Cancelled, Durability, Setter};
use tree_sitter::Node;

use super::files::Files;
use super::root_db::RootDb;
use crate::ast::kinds::NodeKind;
use crate::ast::{self, AstNode, NodeRange};
use crate::capabilities::diagnostics::lsp_diagnostics;
use crate::hir::body_map::ByteOffset;
use crate::ir::def_map::DefId;

use crate::utilities::to_rope_idx;
use crate::workspace::{LoadedFile, Package, PackageId, Workspace};

#[macro_export]
macro_rules! map_def_id {

    (
        match $id_map:expr, $file:expr, $node:expr, $container:expr => {
            $($node_kind:ident => $kind:ident => $id_kind:ident $(=> $break:ident)?),* $(,)?
        }
    ) => {
        match $node.kind_id().into() {
            $(NodeKind::$node_kind => {
                let id = $id_map.id_of_node::<ast::$kind>($node).unwrap();
                let def_id = $crate::ir::def_map::DefId::$kind($crate::ast::$id_kind{file:$file,id});
                $container.push(def_id);
                $($break;)?
            },)*
            _ => {}
        }
    };
}


pub type FileId = File;
#[salsa::input]
#[derive(Debug)]
pub struct File {
    pub text: Rope,
    #[returns(ref)]
    pub path: Utf8PathBuf,
}

pub type SourceRootId = SourceRoot;
#[salsa::input]
#[derive(Debug)]
pub struct SourceRoot {
    #[returns(ref)]
    pub root: Utf8PathBuf,
    pub package_id: PackageId,
    #[returns(ref)]
    pub files: Arc<[FileId]>,
    pub is_dependency: bool,
}

#[salsa::input(singleton)]
pub struct Packages {
    #[returns(ref)]
    pub packages: Arc<[Package]>,
}

#[salsa::db]
#[derive(Default)]
pub struct Db {
    storage: salsa::Storage<Self>,
    files: Arc<Files>,
    // Diagnostics introduced worker threads, which means the current incremental parser design
    // will serialize "concurrent" parsing between worker threads performing independent work
    // as each thread will have to block on the mutex to access the parser.
    // will revisit once i get a design pattern that solves this.
    // parser: Arc<Mutex<IncrementalParser>>
}

#[salsa::db]
impl salsa::Database for Db {}

/// Cache instrumentation is on for the dev binary (the extension's F5 path)
/// and for any build that opts in with `SOLREPR_SALSA_EVENTS`.
fn salsa_events_enabled() -> bool {
    (cfg!(debug_assertions) && !cfg!(test)) || env_flag("SOLREPR_SALSA_EVENTS")
}

/// Cache hits are noisier than misses; opt in separately.
fn salsa_hits_enabled() -> bool {
    env_flag("SOLREPR_SALSA_HITS")
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| value != "0")
}

/// The one cache observer: `WillExecute` is a miss/recompute,
/// `DidValidateMemoizedValue` is a hit.
fn log_salsa_event(event: salsa::Event) {
    use salsa::EventKind;
    match event.kind {
        EventKind::WillExecute { database_key } => {
            tracing::debug!(query = ?database_key, "execute")
        }
        EventKind::DidValidateMemoizedValue { database_key } if salsa_hits_enabled() => {
            tracing::debug!(query = ?database_key, "hit")
        }
        EventKind::WillBlockOn { database_key, .. } => {
            tracing::debug!(query = ?database_key, "blocked")
        }
        EventKind::WillIterateCycle {
            database_key,
            iteration_count,
        } => {
            tracing::warn!(query = ?database_key, iteration_count = ?iteration_count, "cycle")
        }
        _ => {}
    }
}

impl Clone for Db {
    fn clone(&self) -> Self {
        Self {
            storage: self.storage.clone(),
            files: self.files.clone(),
        }
    }
}

impl Db {
    pub(crate) fn new(workspace: Workspace<LoadedFile>) -> Self {
        let storage = if salsa_events_enabled() {
            salsa::Storage::new(Some(Box::new(log_salsa_event)))
        } else {
            salsa::Storage::default()
        };
        Self::with_storage(workspace, storage)
    }

    fn with_storage(workspace: Workspace<LoadedFile>, storage: salsa::Storage<Self>) -> Self {
        let mut salsa = Self {
            storage,
            files: Arc::new(Files::default()),
        };
        salsa.files = Arc::new(Files::new(
            &mut salsa,
            workspace.source_roots,
            workspace.ignored_roots,
        ));

        // Seed the singleton Packages input
        let _ = Packages::builder(workspace.packages.into())
            .durability(Durability::MEDIUM)
            .new(&salsa);

        salsa
    }

    /// A database whose salsa events are collected by `cb` — for cache tests.
    #[cfg(test)]
    pub(crate) fn with_events(
        workspace: Workspace<LoadedFile>,
        cb: impl Fn(salsa::Event) + Send + Sync + 'static,
    ) -> Self {
        Self::with_storage(workspace, salsa::Storage::new(Some(Box::new(cb))))
    }

    pub fn snapshot(&self) -> DbSnapshot {
        DbSnapshot { db: self.clone() }
    }

    pub fn file(&self, path: &Utf8PathBuf) -> Option<File> {
        self.files.file(path)
    }

    pub fn file_source_root(&self, file: FileId) -> Option<SourceRoot> {
        self.files.file_source_root(file)
    }

    pub fn resolve_to_file(&self, from: FileId, rel_path: &str) -> Option<FileId> {
        self.files.resolve_to_file(self, from, rel_path)
    }

    /// `path` sits under a deduplicated library root — outside the
    /// compilation unit. Diagnostics and features stay silent for it.
    pub fn is_ignored_path(&self, path: &Utf8Path) -> bool {
        self.files.is_ignored(path)
    }

    #[allow(dead_code)]
    pub fn set_packages(&mut self, packages: Vec<Package>) {
        Packages::get(self)
            .set_packages(self)
            .with_durability(Durability::MEDIUM)
            .to(packages.into());
    }

    pub fn rope(&self, file: File) -> Rope {
        file.text(self)
    }

    pub fn convert(&self, path: &Utf8PathBuf, position: lsp_types::Position) -> Option<(File, ByteOffset)> {
        let file = self.file(path)?;
        let rope = self.rope(file);
        let char_idx = to_rope_idx(&rope, position);
        let byte_offset = rope.char_to_byte(char_idx);
        Some((file, byte_offset as u32))
    }

    #[allow(dead_code)]
    pub fn node_at_position(
        &self,
        path: &Utf8PathBuf,
        position: lsp_types::Position,
    ) -> Option<AstNode> {
        let (file, offset) = self.convert(path, position)?;
        self.named_node_at(file, offset)
    }

    pub fn named_node_at(&self, file: File, offset: ByteOffset) -> Option<AstNode> {
        let root = self.root(file);
        let range = NodeRange {
            start: offset,
            end: offset,
        };
        root.named_child_node(range)
    }

    #[allow(dead_code)]
    pub fn node_at(&self, file: File, offset: ByteOffset) -> Option<AstNode> {
        let root = self.root(file);
        let range = NodeRange {
            start: offset,
            end: offset,
        };
        root.child_node(range)
    }

    pub fn enclosing_containers(&self, node: Node, file: File) -> Vec<DefId> {
        // Single top-down traversal from root to leaf, recording path

        //TS parent walking is very in-efficient. apparently nodes don't keep parent pointers
        //so every parent call walks the ast from root!.
        let root = self.root(file);
        let root_id = DefId::File(file);
        let mut path = vec![root_id];
        let mut current = root.node();
        while let Some(next) = current.child_with_descendant(node) {
            if next.id() == node.id() {
                break;
            }
            map_def_id! {
                match self.ast_id_map(file), file, next, path => {
                    CONTRACT_DEFINITION => Contract => ContractId,
                    INTERFACE_DEFINITION => Interface => InterfaceId,
                    LIBRARY_DEFINITION => Library => LibraryId,
                    IMPORT_DIRECTIVE => Import => ImportId => break,
                    USING_DIRECTIVE => Using => UsingId => break,
                    USER_DEFINED_TYPE_DEFINITION => Udvt => UdvtId => break,
                    FUNCTION_DEFINITION => Function => FunctionId => break,
                    CONSTRUCTOR_DEFINITION => Function => FunctionId => break,
                    FALLBACK_RECEIVE_DEFINITION => Function => FunctionId => break,
                    MODIFIER_DEFINITION => Modifier => ModifierId => break,
                    STRUCT_DEFINITION => Struct => StructId => break,
                    ENUM_DEFINITION => Enum => EnumId => break,
                    EVENT_DEFINITION => Event => EventId => break,
                    ERROR_DEFINITION => Error => ErrorId => break,
                    STATE_VAR_DECLARATION => Var => VarId => break,
                    CONST_VAR_DECLARATION => Var => VarId => break,
                }
            }
            current = next;
        }
        path
    }

    pub fn open_file(&mut self, path: Utf8PathBuf, text: String) -> Option<File> {
        let files = self.files.clone();
        files.open_file(self, path, text)
    }

    pub fn apply_changes(
        &mut self,
        path: Utf8PathBuf,
        changes: Vec<TextDocumentContentChangeEvent>,
    ) -> Option<File> {
        let files = self.files.clone();
        files.apply_changes(self, path, changes)
    }
}

pub type Cancellable<T> = Result<T, salsa::Cancelled>;

pub struct DbSnapshot {
    db: Db,
}

impl DbSnapshot {
    
    pub fn lsp_diagnostics(&self, file: File) -> Cancellable<Arc<[LspDiagnostic]>> {
        self.call(|db| lsp_diagnostics(db, file))
    }

    fn call<F, T>(&self, f: F) -> Cancellable<T>
    where
        F: FnOnce(&Db) -> T + std::panic::UnwindSafe,
    {
        Cancelled::catch(AssertUnwindSafe(|| f(&self.db)))
    }
}
