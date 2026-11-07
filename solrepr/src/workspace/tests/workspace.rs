use crate::workspace::{PackageId, discover_workspace, workspace_root};
use camino::Utf8PathBuf;
use lsp_types::InitializeParams;

fn params(value: serde_json::Value) -> InitializeParams {
    serde_json::from_value(value).expect("a valid InitializeParams")
}

/// `rootUri` wins when the client sends it.
#[test]
fn workspace_root_prefers_root_uri() {
    let params = params(serde_json::json!({
        "processId": null,
        "rootUri": "file:///tmp/root",
        "capabilities": {},
        "workspaceFolders": [{ "uri": "file:///tmp/folder", "name": "folder" }],
    }));
    assert_eq!(workspace_root(&params).unwrap(), "/tmp/root");
}

/// `rootUri` is deprecated and may be null, so the first workspace folder
/// is the next candidate.
#[test]
fn workspace_root_falls_back_to_a_workspace_folder() {
    let params = params(serde_json::json!({
        "processId": null,
        "rootUri": null,
        "capabilities": {},
        "workspaceFolders": [{ "uri": "file:///tmp/folder", "name": "folder" }],
    }));
    assert_eq!(workspace_root(&params).unwrap(), "/tmp/folder");
}

/// With neither, the process working directory is used rather than
/// failing the initialize handshake.
#[test]
fn workspace_root_falls_back_to_the_working_directory() {
    let params = params(serde_json::json!({
        "processId": null,
        "rootUri": null,
        "capabilities": {},
        "workspaceFolders": null,
    }));
    let expected = Utf8PathBuf::from_path_buf(std::env::current_dir().unwrap()).unwrap();
    assert_eq!(workspace_root(&params).unwrap(), expected);
}

#[test]
fn nested_duplicate_library_is_skipped_but_unique_transitive_dependency_is_kept() {
    let root = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/workspaces/foundry/nested");
    let discovered = discover_workspace(&root);
    let package_roots: Vec<_> = discovered
        .packages
        .iter()
        .map(|package| package.root.clone())
        .collect();

    // root, dependencies/duplicate, dependencies/dep-a, dependencies/dep-a/lib/unique, dependencies/plain, dependencies/fs
    assert_eq!(discovered.packages.len(), 6);
    assert!(
        package_roots
            .iter()
            .any(|root| root.ends_with("dependencies/dep-a/lib/unique"))
    );
    assert!(
        !package_roots
            .iter()
            .any(|root| root.ends_with("dependencies/dep-a/lib/duplicate"))
    );
    assert!(
        !discovered
            .source_roots
            .iter()
            .any(|source| source.root.ends_with("dependencies/dep-a/lib/duplicate/src"))
    );
    // The skipped copy stays on the ignored list — out of the
    // compilation unit but tracked so opened files there stay silent.
    assert!(
        discovered
            .ignored_roots
            .iter()
            .any(|root| root.ends_with("dependencies/dep-a/lib/duplicate"))
    );

    // The custom libs dir still synthesizes a prefix remapping per package.
    let root_package = discovered
        .packages
        .iter()
        .find(|package| package.root == root)
        .expect("root package");
    assert!(
        root_package
            .remappings
            .iter()
            .any(|remapping| remapping.prefix == "dep-a/")
    );
}

#[test]
fn discovers_hardhat_fixture_sources() {
    let root = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/workspaces/hardhat/basic");
    let discovered = discover_workspace(&root);

    assert_eq!(discovered.packages.len(), 1);
    assert!(discovered.source_roots.iter().any(|source| {
        source.root.ends_with("contracts") && !source.files.is_empty()
    }));
}

#[test]
fn discovers_hardhat_and_foundry_sources_in_hybrid_fixture() {
    let root = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/workspaces/hybrid");
    let discovered = discover_workspace(&root);
    let roots: Vec<_> = discovered
        .source_roots
        .iter()
        .map(|source| source.root.file_name().unwrap_or_default())
        .collect();

    assert_eq!(discovered.packages.len(), 1);
    assert!(roots.contains(&"contracts"));
    assert!(roots.contains(&"src"));
}

#[test]
fn standalone_fixture_gets_a_frameworkless_package_and_source_root() {
    let root = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/workspaces/standalone/basic");
    let discovered = discover_workspace(&root);

    assert_eq!(discovered.packages.len(), 1);
    assert_eq!(discovered.source_roots.len(), 1);
    let source = &discovered.source_roots[0];
    assert_eq!(source.package_id, PackageId(0));
    assert!(!source.is_dependency);
    assert!(
        source
            .files
            .iter()
            .any(|file| file.ends_with("StandaloneFixture.sol"))
    );
}

#[test]
fn container_root_indexes_loose_files_without_swallowing_nested_packages() {
    let root = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/workspaces/foundry/container");
    let discovered = discover_workspace(&root);

    // The manifestless root and the nested foundry package coexist.
    assert_eq!(discovered.packages.len(), 2);
    let root_source = discovered
        .source_roots
        .iter()
        .find(|source| source.root == root)
        .expect("a frameworkless source root at the container root");
    assert!(
        root_source
            .files
            .iter()
            .any(|file| file.ends_with("Loose.sol"))
    );
    // The nested package's files stay under its own source root.
    assert!(!root_source.files.iter().any(|file| file.ends_with("P.sol")));
    assert!(
        discovered
            .source_roots
            .iter()
            .any(|source| source.root.ends_with("a/b/inner/src"))
    );
}

#[test]
fn remappings_only_root_keeps_remappings_and_indexes_vendored_files() {
    let root = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/workspaces/foundry/remappings/vendor");
    let discovered = discover_workspace(&root);

    let package = discovered
        .packages
        .iter()
        .find(|package| package.root == root)
        .expect("the remappings.txt root is a package");
    assert!(
        package
            .remappings
            .iter()
            .any(|remapping| remapping.prefix == "dep/")
    );
    // The remapping names vendor/dep — a dependency package of its own,
    // not part of the root's source root.
    let dependency = discovered
        .source_roots
        .iter()
        .find(|source| source.is_dependency && source.root.ends_with("vendor/dep"))
        .expect("vendor/dep is a dependency source root");
    assert!(dependency.root.ends_with("vendor/dep"));
    assert!(
        dependency
            .files
            .iter()
            .any(|file| file.ends_with("Dep.sol"))
    );
    let source = discovered
        .source_roots
        .iter()
        .find(|source| source.root == root)
        .expect("the package root is the source root");
    assert!(!source.files.iter().any(|file| file.ends_with("Dep.sol")));
}

#[test]
fn external_remapping_target_becomes_a_dependency_source_root() {
    let root = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/workspaces/foundry/remappings/external/proj");
    let discovered = discover_workspace(&root);

    let dependency = discovered
        .source_roots
        .iter()
        .find(|source| source.is_dependency && source.root.ends_with("ext_lib"))
        .expect("the external remapping target is a dependency source root");
    assert!(dependency.root.ends_with("ext_lib"));
    assert!(
        dependency
            .files
            .iter()
            .any(|file| file.ends_with("Ext.sol"))
    );
}

#[test]
fn empty_workspace_root_keeps_a_fallback_source_root() {
    let root =
        Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/workspaces/standalone/empty");
    let discovered = discover_workspace(&root);

    assert_eq!(discovered.packages.len(), 1);
    assert_eq!(discovered.source_roots.len(), 1);
    assert!(!discovered.source_roots[0].is_dependency);
    assert!(discovered.source_roots[0].files.is_empty());
}

#[test]
fn hardhat_imports_dir_discovers_vendored_dependency_and_synthesizes_remapping() {
    let root = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/workspaces/hardhat/imports");
    let discovered = discover_workspace(&root);

    // root + vendor/vendored-dep
    assert_eq!(discovered.packages.len(), 2);
    assert!(
        discovered
            .packages
            .iter()
            .any(|package| package.root.ends_with("vendor/vendored-dep"))
    );

    // The configured (non-default) sources/tests dirs are source roots.
    let source_roots: Vec<_> = discovered
        .source_roots
        .iter()
        .filter(|source| !source.is_dependency)
        .map(|source| source.root.file_name().unwrap_or_default())
        .collect();
    assert!(source_roots.contains(&"src"));
    assert!(source_roots.contains(&"spec"));

    let root_package = discovered
        .packages
        .iter()
        .find(|package| package.root.ends_with("hardhat/imports"))
        .expect("root package");
    let remapping = root_package
        .remappings
        .iter()
        .find(|remapping| remapping.prefix == "vendored-dep/")
        .expect("synthesized vendored-dep/ remapping");
    assert!(remapping.target.ends_with("vendor/vendored-dep/src"));
}

#[test]
fn npm_workspace_globs_discover_all_members() {
    let root = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/workspaces/standalone/npm_workspace");
    let discovered = discover_workspace(&root);

    // root + 6 members via *, {a,b}, literal, and ** patterns.
    assert_eq!(discovered.packages.len(), 7);
    let members: Vec<_> = discovered
        .packages
        .iter()
        .map(|package| package.root.as_str())
        .collect();
    for member in [
        "packages/core",
        "packages/utils",
        "tools/a",
        "tools/b",
        "solitary",
        "deep/nested/tooling",
    ] {
        assert!(
            members.iter().any(|root| root.ends_with(member)),
            "{member} not discovered"
        );
    }
    // Packages under node_modules are npm deps, never workspace members.
    assert!(!members.iter().any(|root| root.contains("node_modules")));
}

/// A file selected as the workspace root discovers its parent directory
/// as the package — manifest, remappings, and nested packages included.
#[test]
fn file_root_discovers_the_parent_package() {
    let file = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/workspaces/foundry/remappings/external/proj/Main.sol");
    let discovered = discover_workspace(&file);

    let proj = discovered
        .packages
        .iter()
        .find(|package| package.root.ends_with("proj"))
        .expect("the file's parent directory is a package");
    assert!(
        proj.remappings
            .iter()
            .any(|remapping| remapping.prefix == "ext/"),
        "the parent's remappings.txt is read"
    );
    assert!(discovered.source_roots.iter().any(|source| {
        source.is_dependency && source.root.ends_with("ext_lib")
    }));
    assert!(
        discovered
            .source_roots
            .iter()
            .flat_map(|source| source.files.iter())
            .any(|file| file.ends_with("Main.sol"))
    );
}

/// A vendored library dir is a named dependency edge — trusted like a
/// remapping target — so a manifestless child is a package of its own.
#[test]
fn manifestless_library_child_is_a_named_dependency() {
    let root = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/workspaces/foundry/nested");
    let discovered = discover_workspace(&root);

    let dependency = discovered
        .source_roots
        .iter()
        .find(|source| source.is_dependency && source.root.ends_with("dependencies/plain"))
        .expect("the manifestless library is a dependency source root");
    assert!(dependency.root.ends_with("dependencies/plain"));
    assert!(
        dependency
            .files
            .iter()
            .any(|file| file.ends_with("Plain.sol"))
    );
}

/// Manifestless intermediate directories stay transparent: discovery
/// descends to the real package, and loose sources at the root still get
/// an owner through the frameworkless supplement.
#[test]
fn nested_manifest_keeps_container_dirs_transparent() {
    let root = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/workspaces/foundry/container");
    let discovered = discover_workspace(&root);

    // The nested package and the frameworkless root — the intermediate
    // a/b directories never became packages.
    assert_eq!(discovered.packages.len(), 2);
    assert!(
        discovered
            .packages
            .iter()
            .any(|package| package.root.ends_with("a/b/inner"))
    );
    let source = discovered
        .source_roots
        .iter()
        .find(|source| source.root == root)
        .expect("a frameworkless source root owns the loose file");
    assert!(source.files.iter().any(|file| file.ends_with("Loose.sol")));
    assert!(!source.files.iter().any(|file| file.ends_with("P.sol")));
}

/// `fs/=lib/fs/src/` resolves to the `lib/fs` package root — the same
/// root the library scan produced, so the graph deduplicates it.
#[test]
fn remapping_into_a_source_dir_resolves_to_the_package_root() {
    let root = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/workspaces/foundry/nested");
    let discovered = discover_workspace(&root);

    assert_eq!(
        discovered
            .packages
            .iter()
            .filter(|package| package.root.ends_with("dependencies/fs"))
            .count(),
        1,
        "the src-level remapping target deduplicates with the library package"
    );
    let dependency = discovered
        .source_roots
        .iter()
        .find(|source| source.is_dependency && source.root.ends_with("dependencies/fs"))
        .expect("dependencies/fs is a dependency source root");
    assert!(dependency.files.iter().any(|file| file.ends_with("Lib.sol")));
}

/// Remapping dependencies are traversed transitively: `ext_a`'s own
/// remappings.txt carries the graph to `ext_b`.
#[test]
fn remapping_dependencies_are_traversed_transitively() {
    let root = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/workspaces/foundry/remappings/external/proj");
    let discovered = discover_workspace(&root);

    assert!(discovered.source_roots.iter().any(|source| {
        source.is_dependency && source.root.ends_with("ext_a")
    }));
    assert!(discovered.source_roots.iter().any(|source| {
        source.is_dependency && source.root.ends_with("ext_b")
    }));
}

/// A remapping edge into a deduplicated library copy dies with the
/// copy — the dependency stays out of the compilation unit.
#[test]
fn remapping_into_an_ignored_library_copy_stays_ignored() {
    let root = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/workspaces/foundry/nested");
    let discovered = discover_workspace(&root);

    assert!(
        discovered
            .ignored_roots
            .iter()
            .any(|root| root.ends_with("dependencies/dep-a/lib/duplicate"))
    );
    assert!(
        !discovered
            .packages
            .iter()
            .any(|package| package.root.ends_with("dep-a/lib/duplicate"))
    );
    assert!(!discovered.source_roots.iter().any(|source| {
        source.root.as_str().contains("dep-a/lib/duplicate")
    }));
}

/// A dependency's `test`/`script` dirs are outside the compilation unit:
/// they land on the ignored list and are pruned even when the dep has no
/// source dirs and indexes from its package root.
#[test]
fn dependency_dev_dirs_are_ignored_and_not_indexed() {
    let root = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/workspaces/foundry/dev_dirs");
    let discovered = discover_workspace(&root);

    assert!(
        discovered
            .ignored_roots
            .iter()
            .any(|root| root.ends_with("lib/dep/test"))
    );
    assert!(
        discovered
            .ignored_roots
            .iter()
            .any(|root| root.ends_with("lib/dep/script"))
    );

    let dep = discovered
        .source_roots
        .iter()
        .find(|source| source.root.ends_with("lib/dep"))
        .expect("the dep indexes from its package root");
    assert!(dep.is_dependency);
    assert!(dep.files.iter().any(|file| file.ends_with("Dep.sol")));
    assert!(!dep.files.iter().any(|file| file.ends_with("Dep.t.sol")));
    assert!(!dep.files.iter().any(|file| file.ends_with("Deploy.s.sol")));
}
