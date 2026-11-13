//! Reads discovered files. `FileContent` is what the index asks of a file-list
//! entry; `LoadedFile` is the one loaded shape, with `None` when a read failed.

use camino::{Utf8Path, Utf8PathBuf};
use ropey::Rope;
use std::fs::File;

#[cfg(test)]
use super::{RootWithFiles, WorkspaceWithFiles, discover_workspace};

/// An entry in a discovered root's file list that the index can take. The path
/// is asked for separately because the index checks for an existing handle
/// before deciding whether it needs the text at all.
pub(crate) trait FileContent {
    fn path(&self) -> &Utf8Path;
    fn load(&self) -> (Utf8PathBuf, Option<Rope>);
}

impl FileContent for Utf8PathBuf {
    fn path(&self) -> &Utf8Path {
        self
    }

    fn load(&self) -> (Utf8PathBuf, Option<Rope>) {
        (self.clone(), read(self))
    }
}

/// A discovered file whose text was attempted. `None` means the path is part
/// of its root but could not be read, which is different from the path not
/// existing: the index may still hold a handle for it.
#[derive(Debug, Clone)]
pub(crate) struct LoadedFile {
    pub path: Utf8PathBuf,
    pub text: Option<Rope>,
}

impl LoadedFile {
    /// Read `path`, keeping the path whether or not the read succeeds.
    pub(crate) fn read(path: Utf8PathBuf) -> Self {
        let text = read(&path);
        Self { path, text }
    }
}

impl From<Utf8PathBuf> for LoadedFile {
    /// Reads the file: the text is `None` when it cannot be read, which keeps
    /// the path indexed-but-textless rather than dropping it.
    fn from(path: Utf8PathBuf) -> Self {
        Self::read(path)
    }
}

impl From<LoadedFile> for Utf8PathBuf {
    fn from(file: LoadedFile) -> Self {
        file.path
    }
}

impl FileContent for LoadedFile {
    fn path(&self) -> &Utf8Path {
        &self.path
    }

    fn load(&self) -> (Utf8PathBuf, Option<Rope>) {
        (self.path.clone(), self.text.clone())
    }
}

/// `path`'s contents, or `None` when it cannot be read.
pub(crate) fn read(path: &Utf8Path) -> Option<Rope> {
    let mut reader = File::open(path.as_std_path()).ok()?;
    Rope::from_reader(&mut reader).ok()
}

/// Discover the workspace rooted at `root` and read every discovered file.
/// Files that cannot be read are skipped.
#[cfg(test)]
pub(crate) fn load_roots(root: Utf8PathBuf) -> WorkspaceWithFiles<LoadedFile> {
    let discovered = discover_workspace(&root);
    let roots = discovered
        .roots
        .into_iter()
        /*par_iter*/
        .map(|root| RootWithFiles {
            path: root.path,
            package_id: root.package_id,
            is_dependency: root.is_dependency,
            files: root
                .files
                .into_iter()
                .map(LoadedFile::read)
                .collect(),
        })
        .collect();
    WorkspaceWithFiles {
        packages: discovered.packages,
        roots,
        ignored_roots: discovered.ignored_roots,
    }
}


