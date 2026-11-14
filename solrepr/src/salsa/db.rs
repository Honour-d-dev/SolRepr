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

use crate::utilities::logger;
use crate::utilities::{normalize_path, to_rope_idx};
use crate::workspace::diff::WorkspaceDiff;
use crate::workspace::watch::ChangeEvent;
use crate::workspace::{
    LoadedFile, Package, PackageId, Workspace, WorkspaceWithFiles, loader, loader::FileContent,
};

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

/// Bumped by every change to the file index, so a new or removed root
/// invalidates the def maps that resolved through it.
/// Is a direct dependency in [`RootDb::resolve_to_file`].
#[salsa::input(singleton)]
pub struct FilesRevision {
    pub revision: u32,
}

pub(super) fn bump_revision(db: &mut dyn salsa::Database) {
    let revision = FilesRevision::get(db).revision(db);
    FilesRevision::get(db).set_revision(db).to(revision + 1);
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

impl Clone for Db {
    fn clone(&self) -> Self {
        Self {
            storage: self.storage.clone(),
            files: self.files.clone(),
        }
    }
}

impl Db {
    
    pub(crate) fn new<F: FileContent>(workspace: WorkspaceWithFiles<F>) -> Self {
        Self::build(workspace, Self::storage())
    }

    /// A database whose salsa events are collected by `recorder`. (for testing only)
    #[cfg(test)]
    pub(crate) fn with_events<F: FileContent, R>(workspace: WorkspaceWithFiles<F>, recorder: R) -> Self
    where
        R: Fn(salsa::Event) + Send + Sync + 'static,
    {
        Self::build(workspace, salsa::Storage::new(Some(Box::new(recorder))))
    }

    fn build<F: FileContent>(mut workspace: WorkspaceWithFiles<F>, storage: salsa::Storage<Self>) -> Self {
        let mut db = Self {
            storage,
            files: Arc::new(Files::new()),
        };

        // Seed the singleton inputs
        let _ = Packages::builder(std::mem::take(&mut workspace.packages).into())
            .durability(Durability::MEDIUM)
            .new(&db);
        let _ = FilesRevision::builder(0).new(&db);

        db.files = Arc::new(Files::new().add_roots(&mut db, workspace.into()));
        db
    }

    /// Observe salsa only when tracing is actually going to print something:
    /// with no subscriber (release, tests) there is no callback, so salsa
    /// builds no events at all.
    fn storage() -> salsa::Storage<Self> {
        if tracing::enabled!(tracing::Level::ERROR) {
            salsa::Storage::new(Some(Box::new(logger)))
        } else {
            salsa::Storage::default()
        }
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

    pub fn workspace(&self) -> Workspace<()> {
        self.files.workspace(self)
    }

    pub fn reload_workspace(&mut self, diff: WorkspaceDiff<LoadedFile>) {
        if let Some(packages) = &diff.packages {
            self.set_packages(packages.clone());
        }

        let files = self.files.clone();
        let files = files.add_roots(self, diff);
        self.files = Arc::new(files);
    }

    pub fn resolve_to_file(&self, from: FileId, rel_path: &str) -> Option<FileId> {
        self.files.resolve_to_file(self, from, rel_path)
    }

    /// `path` sits under a deduplicated library root — outside the
    /// compilation unit. Diagnostics and features stay silent for it.
    pub fn is_ignored_path(&self, path: &Utf8Path) -> bool {
        self.files.is_ignored(path)
    }

    pub fn ignored_paths(&self) -> &[Utf8PathBuf] {
        self.files.ignored_paths()
    }

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

 
    /// Apply a file change event to the database.
    /// 
    /// This function handles the case where a file is created, updated, or removed.
    /// It's a noop for opened files since the editor owns file text
    pub fn apply_file_change(&mut self, path: &Utf8Path) -> ChangeEvent {
        let files = self.files.clone();
        let path = normalize_path(path);
        let existing = self.files.file(&path);
        if existing.is_some_and(|file| files.is_open(file)) || files.is_ignored(&path) {
            return ChangeEvent::Skipped;
        }

        match (existing, loader::read(&path)) {
            (Some(file), Some(text)) => {
                file.set_text(self).to(text);
                ChangeEvent::Updated(file)
            }
            (Some(_), None) => match files.remove_file(self, &path) {
                Some(file) => ChangeEvent::Removed(file),
                None => ChangeEvent::Skipped,
            },
            (None, Some(text)) => match files.index_file(self, path, text) {
                Some(file) => ChangeEvent::Created(file),
                // On disk, but under no root we know: a dependency directory
                // may have appeared.
                None => ChangeEvent::Unrooted,
            },
            (None, None) => ChangeEvent::Skipped,
        }
    }

    /// The client closed the document: disk is the truth for it again.
    pub fn close_file(&mut self, file: File) {
        self.files.close_file(file);
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
