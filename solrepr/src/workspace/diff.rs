//! The diff between the live root set and a fresh discovery: pure, no database,
//! no filesystem, no file contents.

use camino::Utf8PathBuf;
use rustc_hash::FxHashSet;

use super::{LoadedFile, Package, Root, RootWithFiles, Workspace, WorkspaceWithFiles};

/// What a reload has to do: the structural moves plus the root-level facts that
/// changed. A fact that did not change is absent, so "changed but the same
/// value" cannot be expressed. `F` is the file type, as in `WorkspaceWithFiles`.
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceDiff<F = Utf8PathBuf> {
    /// The roots that appeared, with the files discovery found under them.
    pub roots_added: Vec<RootWithFiles<F>>,
    pub removed: FxHashSet<Utf8PathBuf>,
    /// `Some` when the package list moved. A package is identified by its
    /// root, so position carries no meaning and the list replaces the old one.
    pub packages: Option<Vec<Package>>,
    /// `Some` when the ignored set moved, so the client's view of it is stale.
    pub ignored: Option<Vec<Utf8PathBuf>>,
}

impl<F> WorkspaceDiff<F> {
    pub(crate) fn is_empty(&self) -> bool {
        self.roots_added.is_empty()
            && self.removed.is_empty()
            && self.packages.is_none()
            && self.ignored.is_none()
    }
}

/// The initial load: every root is new, so there is nothing to compare.
impl<F> From<WorkspaceWithFiles<F>> for WorkspaceDiff<F> {
    fn from(workspace: WorkspaceWithFiles<F>) -> Self {
        Self {
            roots_added: workspace.roots,
            removed: FxHashSet::default(),
            packages: Some(workspace.packages),
            ignored: Some(workspace.ignored_roots),
        }
    }
}

impl From<WorkspaceDiff<Utf8PathBuf>> for WorkspaceDiff<LoadedFile> {
    fn from(diff: WorkspaceDiff<Utf8PathBuf>) -> Self {
        Self {
            roots_added: diff.roots_added.into_iter().map(RootWithFiles::from).collect(),
            removed: diff.removed,
            packages: diff.packages,
            ignored: diff.ignored,
        }
    }
}

impl From<WorkspaceDiff<LoadedFile>> for WorkspaceDiff<Utf8PathBuf> {
    fn from(diff: WorkspaceDiff<LoadedFile>) -> Self {
        Self {
            roots_added: diff.roots_added.into_iter().map(RootWithFiles::from).collect(),
            removed: diff.removed,
            packages: diff.packages,
            ignored: diff.ignored,
        }
    }
}

/// A root is the same only when its path, package and dependency flag match, so
/// an ownership change is a replacement, never an in-place edit. `new` is
/// sorted here because discovery order varies; its projection comes back too.
pub(crate) fn diff<F>(old: &Workspace, new: WorkspaceWithFiles<F>) -> (Workspace<()>, WorkspaceDiff<F>) {
    let new = new.sorted();
    let discovered: Workspace<()> = (&new).into();

    let old_roots = old.roots.iter().collect::<FxHashSet<&Root>>();
    let new_roots = discovered.roots.iter().collect::<FxHashSet<&Root>>();

    let roots_removed = old.roots.iter()
        .filter(|root| !new_roots.contains(root))
        .map(|root| root.path.clone())
        .collect();
    let roots_added = new.roots.into_iter()
        .filter(|root| !old_roots.contains(&Root::from(root)))
        .collect();

    let packages = (old.packages != discovered.packages).then_some(new.packages);
    let ignored = (old.ignored_roots != discovered.ignored_roots).then_some(new.ignored_roots);

    let diff = WorkspaceDiff {
        roots_added,
        removed: roots_removed,
        packages,
        ignored,
    };
    (discovered, diff)
}
