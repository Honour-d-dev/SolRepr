//! Discovering one folder: manifests and dependency roots in, source roots and
//! their files out. Merging several folders lives in `mod.rs`.

use std::sync::mpsc;

use camino::{Utf8Path, Utf8PathBuf};
use ignore::{WalkBuilder, WalkState};
use rustc_hash::FxHashSet;

use crate::utilities::normalize_path;

use super::detect::{PackageDescriptor, PRUNED_DIRS};
use super::graph::discover_package_graph;
use super::layout::{merge_remappings, owner_remappings};
use super::{Package, PackageId, RootWithFiles, Workspace, WorkspaceWithFiles};

/// Discover all packages before the Salsa database is built: manifests and
/// Foundry dependency roots, never a blind crawl of `node_modules`.
pub(super) fn discover_workspace(root: &Utf8Path) -> WorkspaceWithFiles {
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

