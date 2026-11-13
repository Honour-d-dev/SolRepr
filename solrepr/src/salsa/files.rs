use camino::{Utf8Path, Utf8PathBuf};
use dashmap::{DashMap, DashSet};
use lsp_types::TextDocumentContentChangeEvent;
use ropey::Rope;
use rustc_hash::{FxBuildHasher, FxHashSet};
use salsa::{Durability, Setter};
use triomphe::Arc;

use super::db::{File, FileId, Packages, SourceRoot, SourceRootId, bump_revision};
use super::root_db::RootDb;
use crate::workspace::diff::WorkspaceDiff;
use crate::workspace::{Root, Workspace, loader::FileContent};
use crate::utilities::{normalize_path, resolve_import, to_rope_idx};

pub type FxDashMap<K, V> = DashMap<K, V, FxBuildHasher>;
pub type FxDashSet<K> = DashSet<K, FxBuildHasher>;

/// The file index: every `File` the server knows about, the source root that
/// owns it, and the root set those roots belong to.
#[derive(Clone)]
pub struct Files {
    files: Arc<FxDashMap<Utf8PathBuf, File>>,
    file_source_root: Arc<FxDashMap<FileId, SourceRootId>>,
    source_roots: Arc<[SourceRoot]>,
    /// Documents the client has open. Their text is the client's.
    opened: Arc<FxDashSet<File>>,
    /// Roots/paths kept out of the compilation unit — deduplicated library copies
    /// and dependency dev dirs. Files beneath them get no `File`, no
    /// diagnostics, and no features.
    ignored: Arc<[Utf8PathBuf]>,
}

impl Default for Files {
    fn default() -> Self {
        Self {
            files: Arc::new(FxDashMap::default()),
            source_roots: Arc::from(Vec::new()),
            file_source_root: Arc::new(FxDashMap::default()),
            opened: Arc::new(FxDashSet::default()),
            ignored: Arc::from(Vec::new()),
        }
    }
}

impl Files {
    pub fn file(&self, path: &Utf8Path) -> Option<File> {
        self.files.get(path).map(|file| *file)
    }

    /// Creates a new and empty FileIndex
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_roots<F: FileContent>(&self, db: &mut dyn RootDb, diff: WorkspaceDiff<F>) -> Files {
        let added: FxHashSet<&Utf8Path> = diff.roots_added.iter().map(|root| root.path.as_path()).collect();
        
        let mut source_roots = Vec::with_capacity(self.source_roots.len() + diff.roots_added.len() - diff.removed.len());

        let roots_changed = !added.is_empty() || !diff.removed.is_empty();

        // Drop all files in a diff.removed root, unless the root is "replaced" i.e it exists in both sets
        for root in self.source_roots.iter() {
            match diff.removed.contains(root.root(db).as_path()) {
                false => source_roots.push(*root),
                true if !added.contains(root.root(db).as_path()) => {
                    for file in root.files(db).iter() {
                        self.files.remove(file.path(db));
                    }
                }
                // A root replaced in place keeps its files.
                // The files are eventually moved into the replacing id, then the root is dropped
                _ => {}
            }
        }

        // Add files for newly discorvered roots, relocating a file if necessary
        for root in diff.roots_added {
            let new = SourceRootId::builder(
                root.path,
                root.package_id,
                Arc::from(Vec::new()),
                root.is_dependency,
            ).durability(Durability::HIGH).new(db);
            let indexed = root.files.iter().filter_map(|file| match self.file(file.path()) {
                    Some(existing) => {
                        if let Some(previous) = self.file_source_root.get(&existing) && !diff.removed.contains((*previous).root(db).as_path()) {
                            self.detach_file(db, *previous, existing);
                        }
                        self.file_source_root.insert(existing, new);
                        Some(existing)
                    }
                    None => {
                        let (path, text) = file.load();
                        Some(self.add_file(db, new, path, text?))
                    }
                }).collect::<Vec<_>>();
            new.set_files(db).with_durability(Durability::HIGH).to(indexed.into());
            source_roots.push(new);
        }

        // Only a path set that moved can change what an import resolves to; a
        // remapping edit settles itself through `Packages`.
        roots_changed.then(|| bump_revision(db));

        Files {
            source_roots: Arc::from(source_roots),
            ignored: match diff.ignored {
                Some(ignored) => Arc::from(ignored),
                None => self.ignored.clone(),
            },
            ..self.clone()
        }
    }

    /// The live facts in the shape discovery produces — the `old` side of a
    /// reload diff. Files are absent because a reload compares roots, never
    /// their file lists.
    pub fn workspace(&self, db: &dyn RootDb) -> Workspace<()> {
        Workspace {
            packages: Packages::get(db).packages(db).to_vec(),
            roots: self
                .source_roots
                .iter()
                .map(|root| Root {
                    path: root.root(db).clone(),
                    package_id: root.package_id(db),
                    files: (),
                    is_dependency: root.is_dependency(db),
                })
                .collect(),
            ignored_roots: self.ignored.to_vec(),
        }
        .sorted()
    }

    pub fn file_source_root(&self, file: FileId) -> Option<SourceRoot> {
        self.file_source_root.get(&file).map(|root| *root)
    }

    /// `path` sits under a root kept out of the compilation unit — never
    /// indexed: no `File`, no source root, no analysis.
    pub fn is_ignored(&self, path: &Utf8Path) -> bool {
        self.ignored.iter().any(|root| path.starts_with(root))
    }

    pub fn ignored_paths(&self) -> &[Utf8PathBuf] {
        &self.ignored
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

    /// Retires a path that vanished from disk. The `file_source_root` entry
    /// stays: a request already holding the handle still resolves its root.
    pub(super) fn remove_file(&self, db: &mut dyn RootDb, path: &Utf8Path) -> Option<File> {
        let (_, file) = self.files.remove(path)?;
        if let Some(root) = self.file_source_root.get(&file) {
            self.detach_file(db, *root, file);
        }
        bump_revision(db);
        Some(file)
    }

    /// Removes `file` from `source_root`'s list. Used when a more specific root
    /// claims a path a broader root used to own.
    fn detach_file(&self, db: &mut dyn salsa::Database, source_root: SourceRootId, file: File) {
        let files = source_root.files(db);
        let remaining = files.iter().copied().filter(|indexed| *indexed != file).collect::<Vec<_>>();
        if remaining.len() != files.len() {
            source_root
                .set_files(db)
                .with_durability(Durability::HIGH)
                .to(remaining.into());
        }
    }

    /// Mints the `File` and indexes it. The caller owns the root's file list,
    /// so a batch can set it once instead of appending per file.
    fn add_file(&self, db: &mut dyn salsa::Database, source_root: SourceRootId, path: Utf8PathBuf, text: Rope) -> File {
        let durability = if source_root.is_dependency(db) {
            Durability::HIGH
        } else {
            Durability::LOW
        };
        let file = File::builder(text, path.clone()).durability(durability).new(db);
        self.files.insert(path, file);
        self.file_source_root.insert(file, source_root);
        file
    }

    /// Indexes a path that is not indexed yet, under the root that owns it.
    /// The one "a file appeared" operation: `open_file` and a disk create both
    /// end up here.
    pub(super) fn index_file(
        &self,
        db: &mut dyn RootDb,
        path: Utf8PathBuf,
        rope: Rope,
    ) -> Option<File> {
        let source_root = self.path_source_root(db, &path)?;
        let file = self.add_file(db, source_root, path, rope);

        let mut files = source_root.files(db).to_vec();//@TODO maybe we use vec in soureceroots directly
        files.push(file);
        source_root.set_files(db).with_durability(Durability::HIGH).to(files.into());

        // A path that was not resolvable before now is.
        bump_revision(db);
        Some(file)
    }

    pub fn open_file(
        &self,
        db: &mut dyn RootDb,
        path: Utf8PathBuf,
        text: String,
    ) -> Option<File> {
        let path = normalize_path(&path);
        let rope = Rope::from_str(&text);
        let file = if let Some(file) = self.file(&path) {
            // file already exixst, update with editor version
            file.set_text(db).to(rope);
            file
        } else {
            // new file, index it under the root that owns it
            self.index_file(db, path, rope)?
        };
        self.opened.insert(file);
        Some(file)
    }

    /// The document is no longer open: disk is the truth for it again.
    pub(super) fn close_file(&self, file: File) {
        self.opened.remove(&file);
    }

    /// The client owns this document's text, so nothing may read it from disk.
    pub(super) fn is_open(&self, file: File) -> bool {
        self.opened.contains(&file)
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

