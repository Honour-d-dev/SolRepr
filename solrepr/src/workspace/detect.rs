//! Directory-level package detection: turning a directory into a
//! `PackageDescriptor` by reading its manifest, and digesting its source
//! roots, dev dirs, remappings, and outgoing package references in one pass.

use std::fs;

use camino::{Utf8Path, Utf8PathBuf};

use glob::glob;
use rustc_hash::FxHashSet;

use super::parse::{
    parse_foundry_toml, parse_hardhat_config, parse_package_manifest, parse_remappings_txt,
};
use super::Remapping;
use crate::utilities::normalize_path;

pub(super) const PRUNED_DIRS: [&str; 5] = [".git", "node_modules", "out", "artifacts", "cache"];

const HARDHAT_CONFIGS: [&str; 6] = [
    "hardhat.config.ts",
    "hardhat.config.cts",
    "hardhat.config.mts",
    "hardhat.config.js",
    "hardhat.config.cjs",
    "hardhat.config.mjs",
];

/// A file whose contents decide the shape of a package. Editing one can add,
/// move, or remove source roots, so discovery has to run again — unlike a
/// `.sol` file, which is one path in or out of an existing root.
pub(crate) fn is_manifest(path: &Utf8Path) -> bool {
    let Some(name) = path.file_name() else {
        return false;
    };
    matches!(name, "foundry.toml" | "remappings.txt" | "package.json")
        || HARDHAT_CONFIGS.contains(&name)
}

/// Basenames that conventionally hold a package's Solidity sources — a
/// remapping target ending in one names the parent as the package root.
const SOURCE_DIR_NAMES: [&str; 5] = ["src", "contracts", "test", "tests", "script"];

/// A package digested from its manifests: source roots, dev dirs, remappings
/// and outgoing edges. Foundry and Hardhat config can coexist, so this records
/// the resolved facts rather than one package kind.
#[derive(Debug, Clone, Default)]
pub(super) struct PackageDescriptor {
    pub root: Utf8PathBuf,
    pub is_dependency: bool,
    /// Resolved source roots: configured source dirs (foundry `src` plus
    /// `contracts`, hardhat `sources`) plus dev dirs for workspace packages,
    /// or the default fallback when none exist.
    pub source_roots: Vec<Utf8PathBuf>,
    /// Configured dev dirs (foundry `test`/`script`, hardhat `tests`) when
    /// `is_dependency` — on disk but outside the compilation unit. Empty for
    /// workspace packages, whose dev dirs are source roots.
    pub ignored_dirs: Vec<Utf8PathBuf>,
    /// The completed remapping table: declared remappings plus npm dependency
    /// and synthesized library remappings, sorted and deduplicated.
    pub remappings: Vec<Remapping>,
    /// Outgoing package-graph edges: workspace globs, npm dependencies,
    /// vendored libraries, and declared remapping targets.
    pub references: Vec<PackageReference>,
}

#[derive(Debug, Clone)]
pub(super) enum PackageReference {
    Workspace(Utf8PathBuf),
    Dependency(Utf8PathBuf),
    /// A vendored library child (foundry `libs`, hardhat `imports`): `name`
    /// is its directory name — an enclosing package owning a library with
    /// the same name shadows this copy.
    Library { name: String, root: Utf8PathBuf },
    /// A declared remapping target: `root` is the package it resolves to,
    /// `target` the directory the remapping actually points at.
    Remapped {
        root: Utf8PathBuf,
        target: Utf8PathBuf,
    },
}

pub(super) fn find_package_roots( root: &Utf8Path, is_dependency: bool) -> Vec<PackageDescriptor> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_owned()];

    while let Some(dir) = stack.pop() {
        if let Some(descriptor) = detect_package(&dir, is_dependency) {
            out.push(descriptor);
            continue;
        }

        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if let Ok(ft) = entry.file_type()
                && ft.is_dir()
                && let Ok(path) = Utf8PathBuf::from_path_buf(entry.path())
                && let Some(name) = path.file_name()
                && !PRUNED_DIRS.contains(&name)
            {
                stack.push(path);
            }
        }
    }

    // Sort so the initial graph-traversal order does not depend on readdir order.
    out.sort_by(|left, right| left.root.cmp(&right.root));
    out
}

/// The descriptor at `root`: the manifest's digest when present, else a
/// manifestless package trusted by the edge that named it (a configured
/// library dir, an npm dependency, or a remapping target).
pub(super) fn package_descriptor(root: &Utf8Path, is_dependency: bool) -> PackageDescriptor {
    remapped_descriptor(root, is_dependency, None)
}

/// `package_descriptor` for a `Remapped` edge: when the resolved root has no
/// manifest, the remapping `target` subtree is its source root — the target
/// subtree is exactly what the remapping exposes.
pub(super) fn remapped_descriptor(
    root: &Utf8Path,
    is_dependency: bool,
    target: Option<Utf8PathBuf>,
) -> PackageDescriptor {
    detect_package(root, is_dependency).unwrap_or_else(|| {
        let root = normalize_path(root);
        let source_roots = target
            .map(|target| vec![target])
            .unwrap_or_else(|| default_source_roots(&root, is_dependency));
        PackageDescriptor {
            root,
            is_dependency,
            source_roots,
            ..PackageDescriptor::default()
        }
    })
}

/// The fallback source roots when a package configures none: dependencies
/// index from their package root; workspace packages try the conventional
/// `contracts/` dir first, then the root itself.
fn default_source_roots(root: &Utf8Path, is_dependency: bool) -> Vec<Utf8PathBuf> {
    let candidates: &[&str] = if is_dependency {
        &[""]
    } else {
        &["contracts", ""]
    };
    candidates
        .iter()
        .map(|dir| root.join(dir))
        .find(|dir| dir.is_dir())
        .into_iter()
        .collect()
}

pub(super) fn detect_package(dir: &Utf8Path, is_dependency: bool) -> Option<PackageDescriptor> {
    // Manifest-bearing directories are packages; manifestless directories
    // stay transparent so the enclosing walk keeps descending through them.
    manifests_in(dir).map(|manifests| manifest_package(dir, is_dependency, manifests))
}

/// The manifest files present at a directory — any subset can coexist.
struct Manifests {
    foundry_toml: Option<Utf8PathBuf>,
    hardhat_config: Option<Utf8PathBuf>,
    package_json: Option<Utf8PathBuf>,
    remappings_txt: Option<Utf8PathBuf>,
}

impl Manifests {
    fn is_empty(&self) -> bool {
        self.foundry_toml.is_none()
            && self.hardhat_config.is_none()
            && self.package_json.is_none()
            && self.remappings_txt.is_none()
    }
}

/// `foundry.toml`, a `hardhat.config.*`, `package.json`, or `remappings.txt`
/// directly in `dir` — the files found, so `None` means no manifest.
fn manifests_in(dir: &Utf8Path) -> Option<Manifests> {
    let manifests = Manifests {
        foundry_toml: existing_file(dir, "foundry.toml"),
        hardhat_config: HARDHAT_CONFIGS
            .iter()
            .find_map(|name| existing_file(dir, name)),
        package_json: existing_file(dir, "package.json"),
        remappings_txt: existing_file(dir, "remappings.txt"),
    };
    (!manifests.is_empty()).then_some(manifests)
}

/// The digested package at a manifest-bearing root: both configs' facts,
/// resolved into one descriptor.
fn manifest_package(dir: &Utf8Path, is_dependency: bool, manifests: Manifests) -> PackageDescriptor {
    let root = normalize_path(dir);
    let foundry = manifests.foundry_toml.as_ref().and_then(|path| {
        fs::read_to_string(path)
            .ok()
            .and_then(|text| parse_foundry_toml(&text, &root))
    });
    let hardhat = manifests.hardhat_config.as_deref().map(parse_hardhat_config);
    // `remappings.txt` overrides foundry.toml entirely — resolved here.
    let mut remappings = manifests.remappings_txt
        .as_deref()
        .and_then(parse_remappings_txt)
        .unwrap_or_else(|| {
            foundry
                .as_ref()
                .map(|foundry| foundry.remappings.clone())
                .unwrap_or_default()
        });
    let manifest = manifests.package_json.as_deref().and_then(parse_package_manifest);
    let resolved_dependencies = manifest
        .as_ref()
        .map(|manifest| {
            manifest
                .dependency_names
                .iter()
                .filter_map(|name| {
                    resolve_node_package(&root, name).map(|root| (name.clone(), root))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let foundry_libraries = foundry
        .as_ref()
        .map(|foundry| libraries_under(&root, &foundry.libs))
        .unwrap_or_default();
    let hardhat_libraries = hardhat
        .as_ref()
        .map(|hardhat| libraries_under(&root, &hardhat.imports))
        .unwrap_or_default();

    // Outgoing package edges. Only *declared* remappings become `Remapped`
    // edges — the npm and library remappings synthesized below resolve
    // through the Dependency and Library edges already recorded here.
    let mut references = Vec::new();
    for (name, child) in foundry_libraries.iter().chain(&hardhat_libraries) {
        references.push(PackageReference::Library {
            name: name.clone(),
            root: child.clone(),
        });
    }
    references.extend(workspace_references(
        &root,
        manifest
            .as_ref()
            .map(|manifest| manifest.workspace_patterns.as_slice())
            .unwrap_or_default(),
    ));
    references.extend(
        resolved_dependencies
            .iter()
            .map(|(_, root)| PackageReference::Dependency(root.clone())),
    );
    references.extend(remappings.iter().filter_map(|remapping| {
        Some(PackageReference::Remapped {
            root: resolve_package_root(&root, &remapping.target)?,
            target: remapping.target.clone(),
        })
    }));

    // forge and hardhat synthesize `<dir>/ -> <child src>` for vendored
    // libraries; explicit remappings claim their prefixes first.
    for (name, dependency_root) in &resolved_dependencies {
        remappings.push(Remapping {
            context: None,
            prefix: format!("{name}/"),
            target: dependency_root.clone(),
        });
    }
    let mut claimed: FxHashSet<String> = remappings
        .iter()
        .filter(|remapping| remapping.context.is_none())
        .map(|remapping| remapping.prefix.clone())
        .collect();
    if foundry
        .as_ref()
        .is_some_and(|foundry| foundry.auto_detect_remappings)
    {
        synthesize_library_remappings(&foundry_libraries, &mut remappings, &mut claimed);
    }
    synthesize_library_remappings(&hardhat_libraries, &mut remappings, &mut claimed);
    remappings.sort_by(|left, right| {
        left.context
            .cmp(&right.context)
            .then(left.prefix.cmp(&right.prefix))
            .then(left.target.cmp(&right.target))
    });
    remappings.dedup();

    // Dev dirs are source roots for workspace packages; a dependency's tests
    // and scripts are outside the compilation unit entirely.
    let mut source_roots = Vec::new();
    let mut dev_dirs = Vec::new();
    if let Some(foundry) = &foundry {
        // `contracts` is a conventional extra root, not a forge key.
        source_roots.extend(dirs_under(&root, [foundry.src.as_str(), "contracts"]));
        dev_dirs.extend(dirs_under(
            &root,
            [foundry.test.as_str(), foundry.script.as_str()],
        ));
    }
    if let Some(hardhat) = &hardhat {
        source_roots.extend(dirs_under(&root, &hardhat.sources));
        dev_dirs.extend(dirs_under(&root, &hardhat.tests));
    }
    let ignored_dirs = if is_dependency {
        dev_dirs
    } else {
        source_roots.extend(dev_dirs);
        Vec::new()
    };
    if source_roots.is_empty() {
        source_roots = default_source_roots(&root, is_dependency);
    }
    source_roots.sort();
    source_roots.dedup();

    PackageDescriptor {
        root,
        is_dependency,
        source_roots,
        ignored_dirs,
        remappings,
        references,
    }
}

/// `Workspace` edges for each npm `workspaces` glob. Globbing for
/// `package.json` encodes the "members must have one" rule; braces are expanded
/// here because `glob` lacks them.
fn workspace_references(root: &Utf8Path, patterns: &[String]) -> Vec<PackageReference> {
    let mut references = Vec::new();
    for pattern in patterns {
        for pattern in expand_braces(pattern) {
            let pattern = format!("{}/{}/package.json", root, pattern.trim_end_matches('/'));
            let mut dirs = glob(&pattern)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|matched| {
                    Utf8PathBuf::from_path_buf(matched)
                        .ok()
                        .and_then(|path| path.parent().map(Utf8Path::to_path_buf))
                })
                // node_modules packages are npm deps, not workspace members.
                .filter(|dir| !dir.components().any(|c| c.as_str() == "node_modules"))
                .map(|dir| normalize_path(&dir))
                .collect::<Vec<_>>();
            dirs.sort();
            references.extend(dirs.into_iter().map(PackageReference::Workspace));
        }
    }
    references
}

/// `<dir-name>/ -> <child source dir>` per immediate child, for forge's
/// auto-detected libraries and hardhat's vendored `imports`. The first
/// remapping to claim a prefix wins.
fn synthesize_library_remappings(
    libraries: &[(String, Utf8PathBuf)],
    remappings: &mut Vec<Remapping>,
    claimed: &mut FxHashSet<String>,
) {
    for (name, child) in libraries {
        let prefix = format!("{name}/");
        if !claimed.insert(prefix.clone()) {
            continue;
        }
        let target = ["src", "contracts"]
            .iter()
            .map(|dir| child.join(dir))
            .find(|dir| dir.is_dir())
            .unwrap_or_else(|| child.clone());
        remappings.push(Remapping {
            context: None,
            prefix,
            target,
        });
    }
}

/// The package root a remapping target belongs to: the nearest manifest-bearing
/// ancestor below the declaring package, else the parent of a conventional
/// source dir, else the target. `None` for a self or enclosing mapping.
fn resolve_package_root(declaring: &Utf8Path, target: &Utf8Path) -> Option<Utf8PathBuf> {
    if !target.is_dir() {
        return None;
    }
    let resolved = target
        .ancestors()
        .take_while(|ancestor| !declaring.starts_with(ancestor))
        .find(|ancestor| manifests_in(ancestor).is_some())
        .map(Utf8Path::to_path_buf)
        .or_else(|| {
            target
                .file_name()
                .filter(|name| SOURCE_DIR_NAMES.contains(name))
                .and_then(|_| target.parent().map(Utf8Path::to_path_buf))
        })
        .unwrap_or_else(|| target.to_path_buf());
    (!declaring.starts_with(&resolved)).then_some(resolved)
}

/// `dir.join(name)` when that file exists.
fn existing_file(dir: &Utf8Path, name: &str) -> Option<Utf8PathBuf> {
    Some(dir.join(name)).filter(|path| path.is_file())
}

/// `dir.join(name)` when that dir exists.
fn existing_dir(root: &Utf8Path, dir: &str) -> Option<Utf8PathBuf> {
    Some(root.join(dir)).filter(|path| path.is_dir())
}

/// Brace alternates expanded out of a glob pattern — `{a,b}` becomes one
/// pattern per alternate. Unbalanced braces pass through untouched.
fn expand_braces(pattern: &str) -> Vec<String> {
    let Some(open) = pattern.find('{') else {
        return vec![pattern.to_string()];
    };
    let Some(close) = pattern[open..].find('}').map(|close| open + close) else {
        return vec![pattern.to_string()];
    };
    let (head, alternates, tail) = (
        &pattern[..open],
        &pattern[open + 1..close],
        &pattern[close + 1..],
    );
    alternates
        .split(',')
        .flat_map(|alternate| expand_braces(&format!("{head}{alternate}{tail}")))
        .collect()
}

/// The configured directories under `root` that exist.
fn dirs_under<S: AsRef<str>>(root: &Utf8Path, dirs: impl IntoIterator<Item = S>) -> Vec<Utf8PathBuf> {
    dirs.into_iter()
        .filter_map(|dir| existing_dir(root, dir.as_ref()))
        .collect()
}

/// `(name, dir)` pairs for the immediate children of each configured
/// directory that exists — the libraries a package can import from.
fn libraries_under(root: &Utf8Path, dirs: &[String]) -> Vec<(String, Utf8PathBuf)> {
    dirs_under(root, dirs)
        .iter()
        .flat_map(|dir| library_children(dir))
        .collect()
}

/// Immediate child directories of a directory, sorted so iteration order is
/// deterministic.
fn library_children(dir: &Utf8Path) -> Vec<(String, Utf8PathBuf)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut children = entries
        .flatten()
        .filter_map(|entry| {
            let path = Utf8PathBuf::from_path_buf(entry.path()).ok()?;
            if !path.is_dir() {
                return None;
            }
            Some((path.file_name()?.to_string(), path))
        })
        .collect::<Vec<_>>();
    children.sort();
    children
}

fn resolve_node_package(owner: &Utf8Path, name: &str) -> Option<Utf8PathBuf> {
    let mut current = Some(owner);
    while let Some(directory) = current {
        let candidate = directory.join("node_modules").join(name);
        if candidate.join("package.json").is_file() {
            return Some(normalize_path(&candidate));
        }
        current = directory.parent();
    }
    None
}
