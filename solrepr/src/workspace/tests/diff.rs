use rustc_hash::FxHashSet;

use crate::paths::NormalizedPath;
use crate::workspace::diff::{WorkspaceDiff, diff};
use crate::workspace::{Package, PackageId, Remapping, Root, RootWithFiles, Workspace, WorkspaceWithFiles};

/// The live facts: what the index currently holds. Files are absent because a
/// reload compares roots, never their file lists.
fn facts(roots: &[(&str, &str)], ignored: &[&str]) -> Workspace<()> {
    Workspace {
        packages: vec![Package {
            root: NormalizedPath::new("/w"),
            remappings: Vec::new(),
        }],
        roots: roots
            .iter()
            .map(|(path, package)| Root {
                path: NormalizedPath::new(*path),
                package_id: PackageId(NormalizedPath::new(*package)),
                files: (),
                is_dependency: false,
            })
            .collect(),
        ignored_roots: ignored.iter().map(NormalizedPath::new).collect(),
    }
    .sorted()
}

/// A fresh discovery: the same facts plus the files under each root.
fn workspace(roots: &[(&str, &str)], ignored: &[&str]) -> WorkspaceWithFiles<NormalizedPath> {
    WorkspaceWithFiles {
        packages: vec![Package {
            root: NormalizedPath::new("/w"),
            remappings: Vec::new(),
        }],
        roots: roots
            .iter()
            .map(|(path, package)| RootWithFiles {
                path: NormalizedPath::new(*path),
                package_id: PackageId(NormalizedPath::new(*package)),
                files: vec![NormalizedPath::new(format!("{path}/A.sol"))],
                is_dependency: false,
            })
            .collect(),
        ignored_roots: ignored.iter().map(NormalizedPath::new).collect(),
    }
}

fn added(diff: &WorkspaceDiff<NormalizedPath>) -> Vec<NormalizedPath> {
    diff.roots_added.iter().map(|root| root.path.clone()).collect()
}

#[test]
fn identical_discoveries_produce_an_empty_diff() {
    let (_, result) = diff(
        &facts(&[("/w", "/w"), ("/w/lib/dep", "/w")], &["/w/lib/dep/test"]),
        workspace(&[("/w", "/w"), ("/w/lib/dep", "/w")], &["/w/lib/dep/test"]),
    );

    assert!(result.is_empty());
    assert!(result.roots_added.is_empty());
    assert!(result.removed.is_empty());
}

#[test]
fn a_new_root_is_added_with_the_files_under_it() {
    let (_, result) = diff(&facts(&[("/w", "/w")], &[]), workspace(&[("/w", "/w"), ("/w/lib/dep", "/w")], &[]));

    assert!(!result.is_empty());
    assert_eq!(added(&result), vec![NormalizedPath::new("/w/lib/dep")]);
    assert_eq!(result.roots_added[0].files, vec![NormalizedPath::new("/w/lib/dep/A.sol")]);
    assert!(result.removed.is_empty());
}

#[test]
fn a_vanished_root_is_removed() {
    let (_, result) = diff(&facts(&[("/w", "/w"), ("/w/lib/dep", "/w")], &[]), workspace(&[("/w", "/w")], &[]));

    assert_eq!(result.removed, FxHashSet::from_iter([NormalizedPath::new("/w/lib/dep")]));
    assert!(result.roots_added.is_empty());
}

/// A root is the same only when its path, package and dependency flag all
/// match, so an ownership change is a replacement rather than an in-place edit.
#[test]
fn a_root_that_changes_owner_is_replaced() {
    let (_, result) = diff(
        &facts(&[("/w", "/w"), ("/w/lib/dep", "/w")], &[]),
        workspace(&[("/w", "/w"), ("/w/lib/dep", "/w/lib/other")], &[]),
    );

    assert!(!result.is_empty());
    assert_eq!(result.removed, FxHashSet::from_iter([NormalizedPath::new("/w/lib/dep")]));
    assert_eq!(added(&result), vec![NormalizedPath::new("/w/lib/dep")]);
}

#[test]
fn a_remapping_edit_is_a_change_with_no_structural_move() {
    let mut old = facts(&[("/w", "/w")], &[]);
    old.packages[0].remappings = vec![Remapping {
        context: None,
        prefix: "dep/".to_string(),
        target: NormalizedPath::new("lib/dep"),
    }];
    let mut new = workspace(&[("/w", "/w")], &[]);
    new.packages[0].remappings = vec![Remapping {
        context: None,
        prefix: "dep/".to_string(),
        target: NormalizedPath::new("lib/other"),
    }];

    let (_, result) = diff(&old, new);

    assert!(!result.is_empty());
    assert!(result.packages.is_some());
    assert!(result.roots_added.is_empty());
    assert!(result.removed.is_empty());
}

#[test]
fn the_ignored_set_travels_whole() {
    let (_, result) = diff(
        &facts(&[("/w", "/w")], &["/w/lib/dep/test"]),
        workspace(&[("/w", "/w")], &["/w/lib/dep/script"]),
    );

    assert!(!result.is_empty());
    assert_eq!(result.ignored, Some(vec![NormalizedPath::new("/w/lib/dep/script")]));
    assert!(result.roots_added.is_empty());
    assert!(result.removed.is_empty());
}
