use camino::Utf8PathBuf;
use rustc_hash::FxHashSet;

use crate::workspace::diff::{WorkspaceDiff, diff};
use crate::workspace::{
    Package, PackageId, Remapping, Root, RootWithFiles, Workspace, WorkspaceWithFiles,
};

/// The live facts: what the index currently holds. Files are absent because a
/// reload compares roots, never their file lists.
fn facts(roots: &[(&str, &str)], ignored: &[&str]) -> Workspace<()> {
    Workspace {
        packages: vec![Package::default()],
        roots: roots
            .iter()
            .map(|(path, package)| Root {
                path: Utf8PathBuf::from(*path),
                package_id: PackageId(Utf8PathBuf::from(*package)),
                files: (),
                is_dependency: false,
            })
            .collect(),
        ignored_roots: ignored.iter().map(Utf8PathBuf::from).collect(),
    }
    .sorted()
}

/// A fresh discovery: the same facts plus the files under each root.
fn workspace(roots: &[(&str, &str)], ignored: &[&str]) -> WorkspaceWithFiles<Utf8PathBuf> {
    WorkspaceWithFiles {
        packages: vec![Package::default()],
        roots: roots
            .iter()
            .map(|(path, package)| RootWithFiles {
                path: Utf8PathBuf::from(*path),
                package_id: PackageId(Utf8PathBuf::from(*package)),
                files: vec![Utf8PathBuf::from(*path).join("A.sol")],
                is_dependency: false,
            })
            .collect(),
        ignored_roots: ignored.iter().map(Utf8PathBuf::from).collect(),
    }
}

fn added(diff: &WorkspaceDiff<Utf8PathBuf>) -> Vec<&str> {
    diff.roots_added
        .iter()
        .map(|root| root.path.as_str())
        .collect()
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
    let (_, result) = diff(
        &facts(&[("/w", "/w")], &[]),
        workspace(&[("/w", "/w"), ("/w/lib/dep", "/w")], &[]),
    );

    assert!(!result.is_empty());
    assert_eq!(added(&result), vec!["/w/lib/dep"]);
    assert_eq!(
        result.roots_added[0].files,
        vec![Utf8PathBuf::from("/w/lib/dep/A.sol")]
    );
    assert!(result.removed.is_empty());
}

#[test]
fn a_vanished_root_is_removed() {
    let (_, result) = diff(
        &facts(&[("/w", "/w"), ("/w/lib/dep", "/w")], &[]),
        workspace(&[("/w", "/w")], &[]),
    );

    assert_eq!(result.removed, FxHashSet::from_iter([Utf8PathBuf::from("/w/lib/dep")]));
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
    assert_eq!(result.removed, FxHashSet::from_iter([Utf8PathBuf::from("/w/lib/dep")]));
    assert_eq!(added(&result), vec!["/w/lib/dep"]);
}

#[test]
fn a_remapping_edit_is_a_change_with_no_structural_move() {
    let mut old = facts(&[("/w", "/w")], &[]);
    old.packages[0].remappings = vec![Remapping {
        context: None,
        prefix: "dep/".to_string(),
        target: Utf8PathBuf::from("lib/dep"),
    }];
    let mut new = workspace(&[("/w", "/w")], &[]);
    new.packages[0].remappings = vec![Remapping {
        context: None,
        prefix: "dep/".to_string(),
        target: Utf8PathBuf::from("lib/other"),
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
    assert_eq!(
        result.ignored,
        Some(vec![Utf8PathBuf::from("/w/lib/dep/script")])
    );
    assert!(result.roots_added.is_empty());
    assert!(result.removed.is_empty());
}
