//! The shape discovery produces: packages, source roots, and the workspace that
//! groups them. Pure data — no filesystem, no database.

use camino::Utf8PathBuf;
use rustc_hash::FxHashSet;

use super::loader::LoadedFile;

/// A package, identified by its root directory rather than its position, so
/// inserting a package renumbers nothing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PackageId(pub Utf8PathBuf);

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Package {
    pub root: Utf8PathBuf,
    pub remappings: Vec<Remapping>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Remapping {
    pub context: Option<Utf8PathBuf>,
    pub prefix: String,
    pub target: Utf8PathBuf,
}

/// A source root at whatever stage the pipeline loaded it to: `F` is the file
/// list, and `()` when only the root-level facts matter.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Root<F = ()> {
    pub path: Utf8PathBuf,
    pub package_id: PackageId,
    pub files: F,
    pub is_dependency: bool,
}

impl<F> From<&Root<F>> for Root<()> {
    fn from(root: &Root<F>) -> Self {
        Self {
            path: root.path.clone(),
            package_id: root.package_id.clone(),
            is_dependency: root.is_dependency,
            files: ()
        }
    }
}

/// A root that carries the files discovery found under it.
pub(crate) type RootWithFiles<T = Utf8PathBuf> = Root<Vec<T>>;


impl<F> RootWithFiles<F> {
    /// Re-stage the files, e.g. paths that were read into `LoadedFile`s.
    pub(crate) fn map_files<G>(self, map: impl FnMut(F) -> G) -> RootWithFiles<G> {
        RootWithFiles {
            path: self.path,
            package_id: self.package_id,
            is_dependency: self.is_dependency,
            files: self.files.into_iter().map(map).collect(),
        }
    }
}

impl From<RootWithFiles<Utf8PathBuf>> for RootWithFiles<LoadedFile> {
    fn from(root: RootWithFiles<Utf8PathBuf>) -> Self {
        root.map_files(LoadedFile::from)
    }
}

impl From<RootWithFiles<LoadedFile>> for RootWithFiles<Utf8PathBuf> {
    fn from(root: RootWithFiles<LoadedFile>) -> Self {
        root.map_files(Utf8PathBuf::from)
    }
}

/// The discovered package set and the source roots each package owns.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Workspace<F = ()> {
    pub packages: Vec<Package>,
    pub roots: Vec<Root<F>>,
    /// Roots kept out of the compilation unit — deduplicated library copies
    /// and dependency dev dirs. On disk but never indexed.
    pub ignored_roots: Vec<Utf8PathBuf>,
}

/// A workspace that carries the files discovery found under each root.
pub(crate) type WorkspaceWithFiles<T = Utf8PathBuf> = Workspace<Vec<T>>;

impl<F> Workspace<F> {
    pub(crate) fn empty() -> Self {
        Self {
            packages: Vec::new(),
            roots: Vec::new(),
            ignored_roots: Vec::new(),
        }
    }

    /// Order-free, so equality compares facts rather than the order discovery
    /// happened to produce.
    pub(crate) fn sorted(mut self) -> Self {
        self.packages.sort_by(|a, b| a.root.cmp(&b.root));
        self.roots.sort_by(|a, b| a.path.cmp(&b.path));
        self.ignored_roots.sort();
        self
    }
}

impl<F> WorkspaceWithFiles<F> {
    /// Combine the discoveries of several workspace folders.
    ///
    /// Folders nest and share dependencies, so this dedupes by identity: the
    /// first folder to claim a path wins.
    pub(crate) fn merge(workspaces: impl IntoIterator<Item = Self>) -> Self {
        let mut packages = Vec::new();
        let mut roots = Vec::new();
        let mut ignored_roots = Vec::new();
        let mut seen_packages = FxHashSet::default();
        let mut seen_roots = FxHashSet::default();

        for workspace in workspaces {
            for package in workspace.packages {
                if seen_packages.insert(package.root.clone()) {
                    packages.push(package);
                }
            }
            for root in workspace.roots {
                if seen_roots.insert(root.path.clone()) {
                    roots.push(root);
                }
            }
            ignored_roots.extend(workspace.ignored_roots);
        }

        ignored_roots
            .retain(|ignored| !roots.iter().any(|root| root.path.starts_with(ignored.as_path())));
        ignored_roots.sort();
        ignored_roots.dedup();

        Self {
            packages,
            roots,
            ignored_roots,
        }
    }

    pub(crate) fn map_files<G>(self, mut map: impl FnMut(F) -> G) -> WorkspaceWithFiles<G> {
        WorkspaceWithFiles {
            packages: self.packages,
            roots: self
                .roots
                .into_iter()
                .map(|root| root.map_files(&mut map))
                .collect(),
            ignored_roots: self.ignored_roots,
        }
    }
}

impl From<WorkspaceWithFiles<Utf8PathBuf>> for WorkspaceWithFiles<LoadedFile> {
    fn from(workspace: WorkspaceWithFiles<Utf8PathBuf>) -> Self {
        workspace.map_files(LoadedFile::from)
    }
}

impl From<WorkspaceWithFiles<LoadedFile>> for WorkspaceWithFiles<Utf8PathBuf> {
    fn from(workspace: WorkspaceWithFiles<LoadedFile>) -> Self {
        workspace.map_files(Utf8PathBuf::from)
    }
}

impl<F> From<&Workspace<F>> for Workspace<()> {
    fn from(workspace: &Workspace<F>) -> Self {
        Self {
            packages: workspace.packages.clone(),
            roots: workspace
                .roots
                .iter()
                .map(Root::from)
                .collect(),
            ignored_roots: workspace.ignored_roots.clone(),
        }
        .sorted()
    }
}
