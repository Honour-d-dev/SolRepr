use camino::{Utf8Path, Utf8PathBuf};
use dashmap::DashMap;
use lsp_types::TextDocumentContentChangeEvent;
use ropey::Rope;
use rustc_hash::FxBuildHasher;
use salsa::{Durability, Setter};
use triomphe::Arc;

use super::db::{File, FileId, SourceRoot, SourceRootId};
use super::root_db::RootDb;
use crate::workspace::{LoadedFile, SourceRoot as WsSourceRoot};
use crate::utilities::{normalize_path, resolve_import, to_rope_idx};

pub type FxDashMap<K, V> = DashMap<K, V, FxBuildHasher>;

#[derive(Clone)]
pub struct Files {
    files: Arc<FxDashMap<Utf8PathBuf, File>>,
    file_source_root: Arc<FxDashMap<FileId, SourceRootId>>,
    source_roots: Arc<[SourceRoot]>,
    /// Roots kept out of the compilation unit — deduplicated library copies
    /// and dependency dev dirs. Files beneath them get no `File`, no
    /// diagnostics, and no features.
    ignored_roots: Arc<[Utf8PathBuf]>,
}

impl Default for Files {
    fn default() -> Self {
        Self {
            files: Arc::new(FxDashMap::default()),
            source_roots: Arc::from(Vec::new()),
            file_source_root: Arc::new(FxDashMap::default()),
            ignored_roots: Arc::from(Vec::new()),
        }
    }
}

impl Files {
    pub fn new(
        db: &mut dyn salsa::Database,
        roots: Vec<WsSourceRoot<LoadedFile>>,
        ignored: Vec<Utf8PathBuf>,
    ) -> Files {
        let this = Files::default();
        let mut source_roots = Vec::new();
        for root in roots {
            let source_root = SourceRootId::builder(
                root.root,
                root.package_id,
                Arc::from(Vec::new()),
                root.is_dependency,
            )
            .durability(Durability::HIGH)
            .new(db);
            source_roots.push(source_root);

            for file in root.files {
                this.add_file(db, source_root, file.path, file.text);
            }
        }

        Files {
            source_roots: Arc::from(source_roots),
            ignored_roots: Arc::from(ignored),
            ..this
        }
    }

    /// Creates the salsa `File`, indexes it under `path` and `source_root`,
    /// and appends it to the root's file list. Dependency files get HIGH
    /// durability — they don't change during a session.
    fn add_file(
        &self,
        db: &mut dyn salsa::Database,
        source_root: SourceRootId,
        path: Utf8PathBuf,
        text: Rope,
    ) -> File {
        let durability = if source_root.is_dependency(db) {
            Durability::HIGH
        } else {
            Durability::LOW
        };
        let file = File::builder(text, path.clone())
            .durability(durability)
            .new(db);
        self.files.insert(path.clone(), file);
        self.file_source_root.insert(file, source_root);

        let mut files = source_root.files(db).to_vec();
        files.push(file);
        source_root
            .set_files(db)
            .with_durability(Durability::HIGH)
            .to(files.into());
        file
    }

    pub fn file(&self, path: &Utf8PathBuf) -> Option<File> {
        self.files.get(path).map(|file| *file)
    }

    pub fn file_source_root(&self, file: FileId) -> Option<SourceRoot> {
        self.file_source_root.get(&file).map(|root| *root)
    }

    /// `path` sits under a root kept out of the compilation unit — never
    /// indexed: no `File`, no source root, no analysis.
    pub fn is_ignored(&self, path: &Utf8Path) -> bool {
        self.ignored_roots.iter().any(|root| path.starts_with(root))
    }

    /// The source root `path` belongs to, or `None` when the path is outside
    /// the compilation unit
    pub fn path_source_root(&self, db: &dyn RootDb, path: &Utf8Path) -> Option<SourceRootId> {
        if self.is_ignored(path) {
            return None;
        }
        self.source_roots
            .iter()
            .filter(|root| path.starts_with(root.root(db)))
            .max_by_key(|root| root.root(db).components().count())
           .copied()
    }

    pub fn insert_file(
        &self,
        db: &mut dyn RootDb,
        path: Utf8PathBuf,
        rope: Rope,
    ) -> Option<File> {
        let source_root = self.path_source_root(db, &path)?;
        Some(self.add_file(db, source_root, path, rope))
    }

    pub fn open_file(
        &self,
        db: &mut dyn RootDb,
        path: Utf8PathBuf,
        text: String,
    ) -> Option<File> {
        let path = normalize_path(&path);
        let rope = Rope::from_str(&text);
        if let Some(file) = self.file(&path) {
            // file already exixst, update with editor version
            file.set_text(db).to(rope);
            Some(file)
        } else {
            // new file, insert into files and source roots
            self.insert_file(db, path, rope)
        }
    }

    pub fn apply_changes(
        &self,
        db: &mut dyn RootDb,
        path: Utf8PathBuf,
        changes: Vec<TextDocumentContentChangeEvent>,
    ) -> Option<File> {
        let path = normalize_path(&path);
        let file = self.file(&path)?;

        for change in changes {
            if let Some(range) = change.range {
                let mut rope = file.text(db);
                let start = to_rope_idx(&rope, range.start);
                let end = to_rope_idx(&rope, range.end);
                rope.remove(start..end);
                rope.insert(start, &change.text);
                file.set_text(db).to(rope);
            } else {
                file.set_text(db).to(Rope::from_str(&change.text));
            }
        }
        Some(file)
    }

    pub fn resolve_to_file(
        &self,
        db: &dyn RootDb,
        from: FileId,
        rel_path: &str,
    ) -> Option<FileId> {
        let source_root = *self.file_source_root.get(&from)?;
        let package = db.package_config(source_root);
        let resolved_path = resolve_import(
            from.path(db),
            rel_path,
            &package.remappings,
        )
        .ok()?;

        self.file(&resolved_path)
    }
}

