//! Workspace discovery and loading: package-graph traversal, framework layout
//! resolution, remapping ownership, and streaming file load.
//!
//! - `detect` turns directories into `PackageDescriptor`s — digested source
//!   roots, remappings, and outgoing package references.
//! - `graph` expands those descriptors breadth-first into the full package set.
//! - `layout` holds the remapping ownership rules for dependencies.
//! - `parse` holds the pure text parsers for the configuration file formats.
//! - `loader` streams the discovered workspace and source-root file contents to
//!   the database on a channel.

mod detect;
mod graph;
mod layout;
pub(crate) mod loader;
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

pub(crate) use loader::LoadedFile;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PackageId(pub usize);

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Remapping {
    pub context: Option<Utf8PathBuf>,
    pub prefix: String,
    pub target: Utf8PathBuf,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Package {
    pub root: Utf8PathBuf,
    pub remappings: Vec<Remapping>,
}

/// A source root at whatever stage the pipeline has loaded it to:
/// `SourceRoot<Utf8PathBuf>` after discovery, `SourceRoot<LoadedFile>` once
/// file contents are read.
#[derive(Debug, Clone)]
pub(crate) struct SourceRoot<F> {
    pub root: Utf8PathBuf,
    pub package_id: PackageId,
    pub files: Vec<F>,
    pub is_dependency: bool,
}

/// The discovered package set and the source roots each package owns.
/// `F` is the file payload stage: `Utf8PathBuf` after discovery,
/// `LoadedFile` once contents are read.
#[derive(Debug, Clone)]
pub(crate) struct Workspace<F> {
    pub packages: Vec<Package>,
    pub source_roots: Vec<SourceRoot<F>>,
    /// Roots kept out of the compilation unit — deduplicated library copies
    /// and dependency dev dirs. On disk but never indexed.
    pub ignored_roots: Vec<Utf8PathBuf>,
}

impl<F> Workspace<F> {
    pub(crate) fn empty() -> Self {
        Self {
            packages: Vec::new(),
            source_roots: Vec::new(),
            ignored_roots: Vec::new(),
        }
    }
}

/// Load the workspace the client initialized against: resolve the root from
/// the initialize params, then read the discovered workspace in.
pub(crate) fn load_workspace( params: &InitializeParams, ) -> anyhow::Result<Workspace<LoadedFile>> {
    Ok(loader::load_root(workspace_root(params)?))
}

/// The directory the workspace is loaded from: `rootUri`, else the first
/// workspace folder, else the process working directory.
///
/// `rootUri` is deprecated in favour of `workspaceFolders` and clients send it
/// as `null`, so it cannot be required.
pub(crate) fn workspace_root(params: &InitializeParams) -> anyhow::Result<Utf8PathBuf> {
    let uri = params.root_uri.clone().or_else(|| {
        params
            .workspace_folders
            .as_ref()?
            .first()
            .map(|folder| folder.uri.clone())
    });
    if let Some(uri) = uri {
        return to_utf8path(&uri);
    }

    let cwd = std::env::current_dir().context("no workspace root and no working directory")?;
    Utf8PathBuf::from_path_buf(cwd)
        .map_err(|path| anyhow::anyhow!("working directory is not valid UTF-8: {path:?}"))
}

/// Discover all packages before the Salsa database is built.
///
/// The traversal follows package manifests and known Foundry dependency roots,
/// but never crawls the workspace's `node_modules` directory blindly. Each
/// resolved package is converted into source roots before DefMaps can query it.
pub(crate) fn discover_workspace(root: &Utf8Path) -> Workspace<Utf8PathBuf> {
    // A selected file's package candidate is its parent directory.
    let root = match normalize_path(root) {
        path if path.is_file() => path
            .parent()
            .map_or_else(|| path.clone(), Utf8Path::to_path_buf),
        path => path,
    };
    let (mut descriptors, ignored) = discover_package_graph(&root);

    // A directory of Solidity sources with no manifest is still a package —
    // a frameworkless workspace whose files are reachable through relative
    // imports. Only the workspace root gets this treatment: manifestless
    // directories deeper in the tree stay transparent so nested packages
    // beneath them are still discovered.
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

    // Files are indexed exactly once: package roots, deduplicated library
    // roots, and source dirs claimed by another package are pruned from
    // every recursive scan.
    let prune: FxHashSet<Utf8PathBuf> = descriptors
        .iter()
        .flat_map(|descriptor| {
            std::iter::once(descriptor.root.clone())
                .chain(descriptor.source_roots.iter().cloned())
        })
        .chain(ignored.iter().cloned())
        .collect();

    // Each non-dependency package owns the remappings for the dependencies that
    // live beneath it. Selecting the nearest enclosing owner (rather than one
    // global root set) keeps sibling monorepo packages from bleeding mappings
    // into one another; hoisted dependencies with no enclosing owner fall back
    // to the workspace root.
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

/// A workspace with no indexable files still needs one package and source
/// root so that `didOpen` can attach a document later. The fallback is rooted
/// at the selected directory or file and is kept even while it holds no
/// Solidity files.
fn add_fallback_package(
    root: &Utf8Path,
    workspace: &mut Workspace<Utf8PathBuf>,
    prune: &FxHashSet<Utf8PathBuf>,
) {
    let package_id = PackageId(workspace.packages.len());
    workspace.source_roots.push(SourceRoot {
        root: root.to_owned(),
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
    workspace: &mut Workspace<Utf8PathBuf>,
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

    let package_id = PackageId(workspace.packages.len());
    let is_dependency = descriptor.is_dependency;

    for (root, root_files) in collected_roots {
        workspace.source_roots.push(SourceRoot {
            root,
            package_id,
            files: root_files,
            is_dependency,
        });
    }

    workspace.packages.push(Package {
        root: descriptor.root,
        remappings: descriptor.remappings,
    });
}

/// Every `.sol` file under `dir`, except inside `prune` directories
/// (package roots, source dirs claimed by other packages, and deduplicated
/// library roots) and the usual dependency/output directories.
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

