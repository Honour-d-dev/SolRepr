//! Workspace discovery: directories in, a package graph and its source roots
//! out. `detect` reads manifests, `graph` walks dependencies, `loader` reads.

mod detect;
pub(crate) mod diff;
mod graph;
mod layout;
pub(crate) mod loader;
pub(crate) mod watch;
pub(crate) mod worker;
mod parse;

#[cfg(test)]
mod tests;

use std::sync::mpsc;

use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use ignore::{WalkBuilder, WalkState};
use lsp_types::InitializeParams;
use rustc_hash::FxHashSet;

use crate::utilities::{normalize_path, to_utf8path};

use detect::{PackageDescriptor, PRUNED_DIRS};
use graph::discover_package_graph;
use layout::{merge_remappings, owner_remappings};

pub(crate) use detect::is_manifest;
pub(crate) use loader::LoadedFile;

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

/// The directories this session loads: every `workspaceFolders` entry, else
/// `rootUri`, else the working directory. A folder the server cannot read is
/// skipped rather than failing the handshake.
pub(crate) fn workspace_roots(params: &InitializeParams) -> anyhow::Result<Box<[Utf8PathBuf]>> {
    let mut roots: Vec<Utf8PathBuf> = Vec::new();
    for folder in params.workspace_folders.as_deref().unwrap_or_default() {
        if let Ok(path) = to_utf8path(&folder.uri) && !roots.contains(&path) {
            roots.push(path);
        }
    }

    if roots.is_empty() {
        match params.root_uri {
            Some(ref uri) => roots.push(to_utf8path(uri)?),
            None => {
                let cwd = std::env::current_dir().context("no workspace root and no working directory")?;
                roots.push(Utf8PathBuf::from_path_buf(cwd).map_err(|path| anyhow::anyhow!("working directory is not valid UTF-8: {path:?}"))?);
            }
        }
    }

    Ok(roots.into_boxed_slice())
}

/// Discover each folder and merge the results. One folder skips the merge,
/// since it has nothing to overlap with.
pub(crate) fn discover_workspaces(roots: &[Utf8PathBuf]) -> WorkspaceWithFiles<Utf8PathBuf> {
    match roots {
        [single] => discover_workspace(single),
        multi => WorkspaceWithFiles::merge(multi.iter().map(|root| discover_workspace(root))),
    }
}

/// Discover all packages before the Salsa database is built: manifests and
/// Foundry dependency roots, never a blind crawl of `node_modules`.
fn discover_workspace(root: &Utf8Path) -> WorkspaceWithFiles {
    // A selected file's package candidate is its parent directory.
    let root = match normalize_path(root) {
        path if path.is_file() => path
            .parent()
            .map_or_else(|| path.clone(), Utf8Path::to_path_buf),
        path => path,
    };
    let (mut descriptors, ignored) = discover_package_graph(&root);

    // A manifestless directory of sources is still a package, but only at the
    // workspace root: deeper ones stay transparent so nested packages are found.
    if !descriptors
        .iter()
        .any(|descriptor| descriptor.root == root)
    {
        descriptors.push(PackageDescriptor {
            root: root.clone(),
            source_roots: vec![root.clone()],
            ..PackageDescriptor::default()
        });
    }

    // Files are indexed once: package roots, deduped library roots and source
    // dirs another package owns are pruned from every scan.
    let prune: FxHashSet<Utf8PathBuf> = descriptors
        .iter()
        .flat_map(|descriptor| {
            std::iter::once(descriptor.root.clone())
                .chain(descriptor.source_roots.iter().cloned())
        })
        .chain(ignored.iter().cloned())
        .collect();

    // Each non-dependency package owns the remappings for the dependencies
    // beneath it, so sibling monorepo packages cannot bleed into each other.
    let owners = descriptors
        .iter()
        .filter(|descriptor| !descriptor.is_dependency)
        .map(|descriptor| (descriptor.root.clone(), descriptor.remappings.clone()))
        .collect::<Vec<_>>();
    let fallback_remappings = owners
        .iter()
        .find(|(owner_root, _)| owner_root == &root)
        .map(|(_, remappings)| remappings.clone())
        .unwrap_or_default();

    let mut workspace = Workspace::empty();
    for mut descriptor in descriptors {
        if descriptor.is_dependency {
            let owner = owner_remappings(&descriptor.root, &owners, &fallback_remappings);
            descriptor.remappings = merge_remappings(owner, descriptor.remappings);
        }
        add_package(&mut workspace, descriptor, &prune);
    }

    if workspace.packages.is_empty() {
        add_fallback_package(&root, &mut workspace, &prune);
    }
    workspace.ignored_roots = ignored;

    workspace
}

/// A workspace with no indexable files still needs one package and source root,
/// so a document opened there has somewhere to attach.
fn add_fallback_package(root: &Utf8Path, workspace: &mut WorkspaceWithFiles, prune: &FxHashSet<Utf8PathBuf>) {
    let package_id = PackageId(root.to_owned());
    workspace.roots.push(RootWithFiles {
        path: root.to_owned(),
        package_id,
        files: collect_sol_files(root, prune),
        is_dependency: false,
    });
    workspace.packages.push(Package {
        root: root.to_owned(),
        remappings: Vec::new(),
    });
}

fn add_package(
    workspace: &mut WorkspaceWithFiles,
    descriptor: PackageDescriptor,
    prune: &FxHashSet<Utf8PathBuf>,
) {
    let collected_roots = descriptor
        .source_roots
        .into_iter()
        .map(|root| {
            let root = normalize_path(&root);
            (root.clone(), collect_sol_files(&root, prune))
        })
        .filter(|(_, files)| !files.is_empty())
        .collect::<Vec<_>>();
    if collected_roots.is_empty() {
        return;
    }

    let package_id = PackageId(descriptor.root.clone());
    let is_dependency = descriptor.is_dependency;

    for (root, root_files) in collected_roots {
        workspace.roots.push(RootWithFiles {
            path: root,
            package_id: package_id.clone(),
            files: root_files,
            is_dependency,
        });
    }

    workspace.packages.push(Package {
        root: descriptor.root,
        remappings: descriptor.remappings,
    });
}

/// Every `.sol` file under `dir`, except inside `prune` directories and the
/// usual dependency and build-output directories.
fn collect_sol_files(dir: &Utf8Path, prune: &FxHashSet<Utf8PathBuf>) -> Vec<Utf8PathBuf> {
    let mut builder = WalkBuilder::new(dir);
    builder.standard_filters(true);
    builder.filter_entry({
        let dir = dir.to_path_buf();
        let prune = prune.clone();
        move |entry| {
            let path = entry.path();
            if path == dir.as_std_path()
                || !entry.file_type().is_some_and(|file_type| file_type.is_dir())
            {
                return true;
            }
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| PRUNED_DIRS.contains(&name))
            {
                return false;
            }
            match Utf8Path::from_path(path) {
                Some(path) => !prune.contains(path) && !prune.contains(&normalize_path(path)),
                None => true,
            }
        }
    });

    let (tx, rx) = mpsc::channel::<Utf8PathBuf>();
    builder.build_parallel().run(|| {
        let tx = tx.clone();
        Box::new(move |result| {
            let Ok(entry) = result else {
                return WalkState::Continue;
            };
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                return WalkState::Continue;
            }
            let path = entry.into_path();
            if path.extension().and_then(|e| e.to_str()) == Some("sol")
                && let Ok(path) = Utf8PathBuf::from_path_buf(path)
            {
                let _ = tx.send(path);
            }
            WalkState::Continue
        })
    });
    drop(tx);

    let mut files: Vec<_> = rx.into_iter().collect();
    files.sort();
    files
}

