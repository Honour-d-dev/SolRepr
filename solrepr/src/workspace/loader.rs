//! Loads the discovered workspace for the database: every source root's
//! files are read with their text into `SourceRoot<LoadedFile>`s.

use camino::{Utf8Path, Utf8PathBuf};
use ropey::Rope;
use std::fs::File;

use super::{SourceRoot, Workspace, discover_workspace};

#[derive(Debug, Clone)]
pub(crate) struct LoadedFile {
    pub path: Utf8PathBuf,
    pub text: Rope,
}

/// Discover the workspace rooted at `root` and read every discovered file.
/// Files that cannot be read are skipped.
pub(crate) fn load_root(root: Utf8PathBuf) -> Workspace<LoadedFile> {
    let discovered = discover_workspace(&root);
    let source_roots = discovered
        .source_roots
        .into_iter()
        /*par_iter*/
        .map(|source_root| SourceRoot {
            root: source_root.root,
            package_id: source_root.package_id,
            is_dependency: source_root.is_dependency,
            files: source_root
                .files
                .iter()
                .filter_map(|path| load_file(path))
                .collect(),
        })
        .collect();
    Workspace {
        packages: discovered.packages,
        source_roots,
        ignored_roots: discovered.ignored_roots,
    }
}

fn load_file(path: &Utf8Path) -> Option<LoadedFile> {
    let mut reader = File::open(path.as_std_path()).ok()?;
    let text = Rope::from_reader(&mut reader).ok()?;
    Some(LoadedFile {
        path: path.to_owned(),
        text,
    })
}
