use camino::Utf8Path;
use dashmap::{DashMap, DashSet};
use lsp_types::TextDocumentContentChangeEvent;
use ropey::Rope;
use rustc_hash::{FxBuildHasher, FxHashMap, FxHashSet};
use salsa::{Durability, Setter};
use triomphe::Arc;

use super::db::{File, FileId, Packages, RootSet, SourceRoot, SourceRootId};
use super::root_db::RootDb;
use crate::paths::NormalizedPath;
use crate::utilities::{resolve_import, to_rope_idx};
use crate::workspace::diff::WorkspaceDiff;
use crate::workspace::{Root, Workspace, loader::FileContent};

pub type FxDashMap<K, V> = DashMap<K, V, FxBuildHasher>;
pub type FxDashSet<K> = DashSet<K, FxBuildHasher>;

/// The path index, the documents the client owns, and the roots kept out of the
/// compilation unit. Everything else about a file lives on its salsa inputs.
#[derive(Clone)]
pub struct Files {
    index: Arc<FxDashMap<NormalizedPath, File>>,
    /// Documents the client has open. Their text is the client's.
    opened: Arc<FxDashSet<File>>,
    /// Roots/paths kept out of the compilation unit — deduplicated library copies
    /// and dependency dev dirs. Files beneath them get no `File`, no
    /// diagnostics, and no features.
    ignored: Arc<[NormalizedPath]>,
}

impl Default for Files {
    fn default() -> Self {
        Self {
            index: Arc::new(FxDashMap::default()),
            opened: Arc::new(FxDashSet::default()),
            ignored: Arc::from(Vec::new()),
        }
    }
}

impl Files {
    pub fn add_roots<F: FileContent>(&self, db: &mut dyn RootDb, diff: WorkspaceDiff<F>) -> Files {
        if !diff.roots_added.is_empty() || !diff.removed.is_empty() {
            let (mut roots, dropped): (Vec<_>, Vec<_>) = self
                .source_roots(db)
                .iter()
                .copied()
                .partition(|root| !diff.removed.contains(root.root(db).as_path()));
            let mut detached: FxHashMap<SourceRootId, FxHashSet<File>> = FxHashMap::default();

            for root in diff.roots_added {
                let new = SourceRoot::builder(root.path, root.package_id, Arc::from(Vec::new()), root.is_dependency)
                    .durability(Durability::HIGH)
                    .files_durability(files_durability(root.is_dependency))
                    .new(db);

                let mut indexed = Vec::with_capacity(root.files.len());
                let mut minted = Vec::new();
                for file in root.files {
                    let file = match self.file(file.path()) {
                        Some(existing) => {
                            let previous = existing.root(db);
                            if !dropped.contains(&previous) {
                                detached.entry(previous).or_default().insert(existing);
                            }
                            existing.set_root(db).to(new);
                            existing
                        }
                        None => {
                            let (path, text) = file.load();
                            let Some(text) = text else { continue };
                            let file = self.mint_file(db, new, path.clone(), text);
                            minted.push((path, file));
                            file
                        }
                    };
                    indexed.push(file);
                }
                if !indexed.is_empty() {
                    // Index writes wait until the input write cannot be cancelled halfway.
                    new.set_files(db).to(indexed.into());
                    for (path, file) in minted {
                        self.index.insert(path, file);
                    }
                }
                roots.push(new);
            }

            for (previous, removed) in detached {
                Files::detach_files(db, previous, |file| !removed.contains(&file));
            }

            // A file no surviving root owns leaves the index. The `File` input stays,
            // so a request already holding the handle still resolves its root.
            for old in dropped {
                let old_files = old.set_files(db).to(Arc::from(Vec::new()));
                for &file in old_files.iter() {
                    if file.root(db) == old {
                        self.index.remove(file.path(db));
                        self.opened.remove(&file);
                    }
                }
            }

            RootSet::get(db).set_roots(db).to(roots.into());
        }

        Files {
            ignored: match diff.ignored {
                Some(ignored) => Arc::from(ignored),
                None => self.ignored.clone(),
            },
            ..self.clone()
        }
    }

    /// The live workspace facts in the shape workspace discovery produces.
    /// Files are absent because a reload compares roots, not their file lists.
    pub fn workspace(&self, db: &dyn RootDb) -> Workspace {
        Workspace {
            packages: Packages::get(db).packages(db).to_vec(),
            roots: self
                .source_roots(db)
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

    // Reads ------------------------------------------------------------------------------------------

    /// The file indexed at `path`.
    pub fn file(&self, path: &Utf8Path) -> Option<File> {
        self.index.get(path).map(|file| *file)
    }

    pub fn source_roots<'db>(&self, db: &'db dyn RootDb) -> &'db [SourceRootId] {
        RootSet::get(db).roots(db)
    }

    /// `path` sits under a root kept out of the compilation unit — never
    /// indexed: no `File`, no source root, no analysis.
    pub fn is_ignored(&self, path: &Utf8Path) -> bool {
        self.ignored.iter().any(|root| path.starts_with(root))
    }

    pub fn ignored_paths(&self) -> &[NormalizedPath] {
        &self.ignored
    }

    /// The root that owns `path`, by the longest enclosing root path.
    fn root_containing(&self, db: &dyn RootDb, roots: &[SourceRootId], path: &NormalizedPath) -> Option<SourceRootId> {
        roots
            .iter()
            .filter_map(|&root| {
                let root_path = root.root(db);
                path.starts_with(root_path).then(|| (root_path.components().count(), root))
            })
            .max_by_key(|(depth, _)| *depth)
            .map(|(_, root)| root)
    }

    /// `candidate`'s file, subscribed to the membership that would change the
    /// answer: the owning root when it exists, the would-be owner when it does
    /// not, so a file appearing or vanishing re-runs the resolution.
    fn indexed(&self, db: &dyn RootDb, candidate: &NormalizedPath) -> Option<File> {
        if let Some(file) = self.file(candidate) {
            file.root(db).files(db);
            return Some(file);
        }
        let roots = self.source_roots(db);
        self.root_containing(db, roots, candidate)?.files(db);
        None
    }

    // Writes ----------------------------------------------------------------------------------------------

    /// Adds a new file on a path that is not indexed yet, under the root that owns it.
    /// The one "a file appeared" operation: `open_file` and a disk create both end up here.
    pub(super) fn add_file(&self, db: &mut dyn RootDb, path: NormalizedPath, rope: Rope) -> Option<File> {
        if self.is_ignored(&path) {
            return None;
        }
        let root = self.root_containing(db, self.source_roots(db), &path)?;
        let file = self.mint_file(db, root, path.clone(), rope);

        let mut files = root.files(db).to_vec();
        files.push(file);
        root.set_files(db).to(files.into());
        self.index.insert(path, file);
        Some(file)
    }

    /// Retires a path that vanished from disk. The `File` input keeps its root,
    /// so a request already holding the handle still resolves it.
    pub(super) fn remove_file(&self, db: &mut dyn RootDb, path: &NormalizedPath) -> Option<File> {
        let file = self.file(path)?;
        let root = file.root(db);
        Files::detach_files(db, root, |indexed| indexed != file);
        self.index.remove(path);
        Some(file)
    }

    fn mint_file(&self, db: &mut dyn salsa::Database, source_root: SourceRootId, path: NormalizedPath, text: Rope) -> File {
        File::builder(text, path, source_root)
            .durability(Durability::HIGH)
            .text_durability(text_durability(source_root.is_dependency(db)))
            .new(db)
    }

    pub fn open_file(&self, db: &mut dyn RootDb, path: NormalizedPath, text: String) -> Option<File> {
        let file = if let Some(file) = self.file(&path) {
            if file.text(db) != text {
                file.set_text(db).to(Rope::from_str(&text));
            }
            file
        } else {
            self.add_file(db, path, Rope::from_str(&text))?
        };
        self.opened.insert(file);
        Some(file)
    }

    fn detach_files(db: &mut dyn salsa::Database, source_root: SourceRootId, mut keep: impl FnMut(File) -> bool) {
        let files = source_root.files(db);
        let remaining = files.iter().copied().filter(|&file| keep(file)).collect::<Vec<_>>();
        if remaining.len() != files.len() {
            source_root.set_files(db).to(remaining.into());
        }
    }

    /// The document is no longer open: disk is the truth for it again.
    pub(super) fn close_file(&self, file: File) {
        self.opened.remove(&file);
    }

    /// The client owns this document's text, so nothing may read it from disk.
    pub(super) fn is_open(&self, file: File) -> bool {
        self.opened.contains(&file)
    }

    pub fn apply_changes(&self, db: &mut dyn RootDb, path: NormalizedPath, changes: Vec<TextDocumentContentChangeEvent>) -> Option<File> {
        let file = self.file(&path)?;
        if changes.is_empty() {
            return Some(file);
        }

        let mut rope = file.text(db);
        for change in changes {
            if let Some(range) = change.range {
                let start = to_rope_idx(&rope, range.start);
                let end = to_rope_idx(&rope, range.end);
                rope.remove(start..end);
                rope.insert(start, &change.text);
            } else {
                rope = Rope::from_str(&change.text);
            }
        }
        file.set_text(db).to(rope);
        Some(file)
    }

    pub fn resolve_to_file(&self, db: &dyn RootDb, from: FileId, rel_path: &str) -> Option<FileId> {
        let package = db.package_config(from.root(db));
        if let Ok(candidate) = resolve_import(from.path(db), rel_path, &package.remappings)
            && let Some(file) = self.indexed(db, &candidate)
        {
            return Some(file);
        }
        if !rel_path.starts_with('.') {
            let candidate = NormalizedPath::new(package.root.join(rel_path));
            return self.indexed(db, &candidate);
        }
        None
    }
}

fn files_durability(is_dependency: bool) -> Durability {
    if is_dependency { Durability::HIGH } else { Durability::MEDIUM }
}

fn text_durability(is_dependency: bool) -> Durability {
    if is_dependency { Durability::HIGH } else { Durability::LOW }
}
