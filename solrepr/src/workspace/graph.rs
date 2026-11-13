//! Package-graph traversal: local and workspace packages, vendored libraries,
//! npm dependencies and remapping targets, deduplicated.

use std::collections::VecDeque;

use camino::{Utf8Path, Utf8PathBuf};
use rustc_hash::{FxHashMap, FxHashSet};

use super::detect::{
    PackageDescriptor, PackageReference, find_package_roots, package_descriptor,
    remapped_descriptor,
};

/// Expand every package source into one deduplicated graph, with the roots kept
/// out of the compilation unit in `ignored`.
///
/// Breadth-first, so an ancestor's claim on a library name skips the nested
/// copy (duplicate OpenZeppelin under `lib/`) while unclaimed ones survive.
pub(super) fn discover_package_graph( root: &Utf8Path ) -> (Vec<PackageDescriptor>, Vec<Utf8PathBuf>) {
    let mut queue = VecDeque::from(find_package_roots(root, false));
    let mut discovered = Vec::new();
    let mut ignored = Vec::new();
    let mut seen = FxHashSet::default();
    // Direct library names owned by each package processed so far. Because the
    // traversal is breadth-first, every ancestor of the package being expanded
    // has already recorded its libraries.
    let mut owned_libraries: FxHashMap<Utf8PathBuf, Vec<String>> = FxHashMap::default();

    while let Some(descriptor) = queue.pop_front() {
        if !seen.insert(descriptor.root.clone()) {
            continue;
        }

        let mut ancestors = None;
        let mut own_libraries = Vec::new();
        for reference in &descriptor.references {
            match reference {
                PackageReference::Workspace(root) => {
                    if !seen.contains(root) {
                        queue.push_back(package_descriptor(root, false));
                    }
                }
                PackageReference::Dependency(root) => {
                    if !seen.contains(root) {
                        queue.push_back(package_descriptor(root, true));
                    }
                }
                PackageReference::Library { name, root } => {
                    let ancestors = ancestors.get_or_insert_with(|| {
                        ancestor_library_names(&descriptor.root, &owned_libraries)
                    });
                    if ancestors.contains(name) {
                        ignored.push(root.clone());
                        continue;
                    }
                    own_libraries.push(name.clone());
                    // A configured library dir is a named edge like a remapping
                    // target, so a manifestless child is a package in its own right.
                    let mut roots = find_package_roots(root, true);
                    if !roots.iter().any(|d| d.root.as_path() == root.as_path()) {
                        roots.push(package_descriptor(root, true));
                    }
                    queue.extend(roots);
                }
                PackageReference::Remapped { root, target } => {
                    if !seen.contains(root) {
                        queue.push_back(remapped_descriptor(root, true, Some(target.clone())));
                    }
                }
            }
        }
        if !own_libraries.is_empty() {
            owned_libraries.insert(descriptor.root.clone(), own_libraries);
        }
        discovered.push(descriptor);
    }

    // A dependency's dev dirs are outside the compilation unit — ignored
    // unless one encloses a discovered package root.
    let dev_dirs = discovered
        .iter()
        .flat_map(|descriptor| descriptor.ignored_dirs.iter())
        .filter(|dir| !discovered.iter().any(|d| d.root.starts_with(dir.as_path())))
        .cloned()
        .collect::<Vec<_>>();
    ignored.extend(dev_dirs);

    // A remapping edge can reach a library copy an ancestor deduplicated only
    // after the edge was queued — filter against the final ignored roots.
    discovered.retain(|descriptor| {
        !ignored
            .iter()
            .any(|root| descriptor.root.starts_with(root))
    });

    (discovered, ignored)
}

/// Library directory names claimed by the packages enclosing `package_root`.
/// A strict ancestor relationship is required, so a package never claims a
/// library against itself.
fn ancestor_library_names(
    package_root: &Utf8Path,
    owned_libraries: &FxHashMap<Utf8PathBuf, Vec<String>>,
) -> FxHashSet<String> {
    owned_libraries
        .iter()
        .filter(|(owner, _)| {
            package_root != owner.as_path() && package_root.starts_with(owner.as_path())
        })
        .flat_map(|(_, names)| names.iter().cloned())
        .collect()
}
