use crate::paths::NormalizedPath;
use crate::utilities::fixture_path;
use crate::workspace::{
    Package, PackageId, RootWithFiles, WorkspaceWithFiles, discover_workspace, discover_workspaces,
    workspace_roots,
};
use camino::Utf8PathBuf;
use lsp_types::{InitializeParams, Url};

fn params(value: serde_json::Value) -> InitializeParams {
    serde_json::from_value(value).expect("a valid InitializeParams")
}

fn folder(path: &camino::Utf8Path) -> serde_json::Value {
    serde_json::json!({
        "uri": Url::from_file_path(path).expect("folder uri").to_string(),
        "name": "folder",
    })
}

/// Every advertised folder is a root: a multi-folder session discovers each
/// one, and the merged result is what the database sees.
#[test]
fn workspace_roots_are_the_advertised_folders() {
    let first = fixture_path("fixtures/workspaces/foundry/nested");
    let second = fixture_path("fixtures/workspaces/foundry/container");
    let params = params(serde_json::json!({
        "processId": null,
        "rootUri": null,
        "capabilities": {},
        "workspaceFolders": [folder(&first), folder(&second)],
    }));

    assert_eq!(
        workspace_roots(&params).unwrap().to_vec(),
        vec![first, second],
        "folders keep the order the client sent"
    );
}

/// `rootUri` is deprecated but still sent by single-folder clients, so it is
/// the fallback when no folder is advertised.
#[test]
fn workspace_roots_fall_back_to_root_uri() {
    let root = fixture_path("fixtures/workspaces/foundry/nested");
    let params = params(serde_json::json!({
        "processId": null,
        "rootUri": Url::from_file_path(&root).unwrap().to_string(),
        "capabilities": {},
        "workspaceFolders": null,
    }));

    assert_eq!(workspace_roots(&params).unwrap().to_vec(), vec![root]);
}

/// A folder the server cannot read is skipped rather than failing the
/// handshake, and an all-unreadable set falls back like an empty one.
#[test]
fn an_unreadable_folder_is_skipped() {
    let params = params(serde_json::json!({
        "processId": null,
        "rootUri": null,
        "capabilities": {},
        "workspaceFolders": [{ "uri": "untitled:Untitled-1", "name": "virtual" }],
    }));
    let expected = Utf8PathBuf::from_path_buf(std::env::current_dir().unwrap()).unwrap();

    assert_eq!(workspace_roots(&params).unwrap().to_vec(), vec![expected]);
}

/// With neither, the process working directory is used rather than failing the
/// initialize handshake.
#[test]
fn workspace_roots_fall_back_to_the_working_directory() {
    let params = params(serde_json::json!({
        "processId": null,
        "rootUri": null,
        "capabilities": {},
        "workspaceFolders": null,
    }));
    let expected = Utf8PathBuf::from_path_buf(std::env::current_dir().unwrap()).unwrap();

    assert_eq!(workspace_roots(&params).unwrap().to_vec(), vec![expected]);
}

/// One folder skips the merge: the discovery is the answer.
#[test]
fn one_folder_skips_the_merge() {
    let root = fixture_path("fixtures/workspaces/foundry/nested");

    assert_eq!(
        discover_workspaces(std::slice::from_ref(&root)),
        discover_workspace(&root)
    );
}

/// Folders nest and share dependencies, so a merge dedupes by identity and lets
/// an opened folder win over another folder's ignored list.
#[test]
fn merging_folders_dedupes_shared_facts() {
    let workspace = |root: &str, package: &str, ignored: &[&str]| WorkspaceWithFiles {
        packages: vec![Package {
            root: NormalizedPath::new(package),
            remappings: Vec::new(),
        }],
        roots: vec![RootWithFiles {
            path: NormalizedPath::new(root),
            package_id: PackageId(NormalizedPath::new(package)),
            files: vec![NormalizedPath::new(format!("{root}/A.sol"))],
            is_dependency: false,
        }],
        ignored_roots: ignored.iter().map(NormalizedPath::new).collect(),
    };

    let outer = workspace("/proj", "/proj", &["/proj/lib/dep", "/proj/lib/other"]);
    let inner = workspace("/proj/lib/dep", "/proj/lib/dep", &[]);
    let merged = WorkspaceWithFiles::merge([outer, inner]);

    assert_eq!(
        merged
            .roots
            .iter()
            .map(|root| root.path.as_str())
            .collect::<Vec<_>>(),
        vec!["/proj", "/proj/lib/dep"],
        "both folders contribute their roots, and a shared path is kept once"
    );
    assert_eq!(merged.packages.len(), 2, "a package is kept once per root");
    assert_eq!(
        merged.ignored_roots,
        vec![Utf8PathBuf::from("/proj/lib/other")],
        "a directory another folder opened is not ignored"
    );
}

#[test]
fn nested_duplicate_library_is_skipped_but_unique_transitive_dependency_is_kept() {
    let root = fixture_path("fixtures/workspaces/foundry/nested");
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
    assert!(!discovered.roots.iter().any(|source| {
        source
            .path
            .ends_with("dependencies/dep-a/lib/duplicate/src")
    }));
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
    let root = fixture_path("fixtures/workspaces/hardhat/basic");
    let discovered = discover_workspace(&root);

    assert_eq!(discovered.packages.len(), 1);
    assert!(
        discovered
            .roots
            .iter()
            .any(|source| { source.path.ends_with("contracts") && !source.files.is_empty() })
    );
}

#[test]
fn discovers_hardhat_and_foundry_sources_in_hybrid_fixture() {
    let root = fixture_path("fixtures/workspaces/hybrid");
    let discovered = discover_workspace(&root);
    let roots: Vec<_> = discovered
        .roots
        .iter()
        .map(|source| source.path.file_name().unwrap_or_default())
        .collect();

    assert_eq!(discovered.packages.len(), 1);
    assert!(roots.contains(&"contracts"));
    assert!(roots.contains(&"src"));
}

#[test]
fn standalone_fixture_gets_a_frameworkless_package_and_source_root() {
    let root = fixture_path("fixtures/workspaces/standalone/basic");
    let discovered = discover_workspace(&root);

    assert_eq!(discovered.packages.len(), 1);
    assert_eq!(discovered.roots.len(), 1);
    let source = &discovered.roots[0];
    assert_eq!(source.package_id, PackageId(root.clone()));
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
    let root = fixture_path("fixtures/workspaces/foundry/container");
    let discovered = discover_workspace(&root);

    // The manifestless root and the nested foundry package coexist.
    assert_eq!(discovered.packages.len(), 2);
    let root_source = discovered
        .roots
        .iter()
        .find(|source| source.path == root)
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
            .roots
            .iter()
            .any(|source| source.path.ends_with("a/b/inner/src"))
    );
}

#[test]
fn remappings_only_root_keeps_remappings_and_indexes_vendored_files() {
    let root = fixture_path("fixtures/workspaces/foundry/remappings/vendor");
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
    // The remapping is resolution data only: `vendor/dep` is inside the root's
    // own source tree, so it is indexed there and never becomes a package.
    assert!(
        discovered
            .packages
            .iter()
            .all(|package| package.root == root),
        "a remapping never creates a package: {:?}",
        discovered.packages
    );
    let source = discovered
        .roots
        .iter()
        .find(|source| source.path == root)
        .expect("the package root is the source root");
    assert!(
        source.files.iter().any(|file| file.ends_with("Dep.sol")),
        "the vendored file is indexed with the package that declares the alias"
    );
}

#[test]
fn empty_workspace_root_keeps_a_fallback_source_root() {
    let root = fixture_path("fixtures/workspaces/standalone/empty");
    let discovered = discover_workspace(&root);

    assert_eq!(discovered.packages.len(), 1);
    assert_eq!(discovered.roots.len(), 1);
    assert!(!discovered.roots[0].is_dependency);
    assert!(discovered.roots[0].files.is_empty());
}

#[test]
fn hardhat_imports_dir_discovers_vendored_dependency_and_synthesizes_remapping() {
    let root = fixture_path("fixtures/workspaces/hardhat/imports");
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
        .roots
        .iter()
        .filter(|source| !source.is_dependency)
        .map(|source| source.path.file_name().unwrap_or_default())
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
    let root = fixture_path("fixtures/workspaces/standalone/npm_workspace");
    let discovered = discover_workspace(&root);

    // root + 6 members via *, {a,b}, literal, and ** patterns.
    assert_eq!(discovered.packages.len(), 7);
    let members: Vec<_> = discovered
        .packages
        .iter()
        .map(|package| package.root.clone())
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
    assert!(
        !members
            .iter()
            .any(|root| root.components().any(|c| c.as_str() == "node_modules"))
    );
}

/// A file selected as the workspace root discovers its parent directory
/// as the package — manifest, remappings, and nested packages included.
#[test]
fn file_root_discovers_the_parent_package() {
    let file = fixture_path("fixtures/workspaces/foundry/remappings/vendor/Main.sol");
    let discovered = discover_workspace(&file);

    let proj = discovered
        .packages
        .iter()
        .find(|package| package.root.ends_with("vendor"))
        .expect("the file's parent directory is a package");
    assert!(
        proj.remappings
            .iter()
            .any(|remapping| remapping.prefix == "dep/"),
        "the parent's remappings.txt is read"
    );
    assert!(
        discovered
            .roots
            .iter()
            .flat_map(|source| source.files.iter())
            .any(|file| file.ends_with("Main.sol"))
    );
}

/// A vendored library dir is a named dependency edge — trusted like a
/// remapping target — so a manifestless child is a package of its own.
#[test]
fn manifestless_library_child_is_a_named_dependency() {
    let root = fixture_path("fixtures/workspaces/foundry/nested");
    let discovered = discover_workspace(&root);

    let dependency = discovered
        .roots
        .iter()
        .find(|source| source.is_dependency && source.path.ends_with("dependencies/plain"))
        .expect("the manifestless library is a dependency source root");
    assert!(dependency.path.ends_with("dependencies/plain"));
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
    let root = fixture_path("fixtures/workspaces/foundry/container");
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
        .roots
        .iter()
        .find(|source| source.path == root)
        .expect("a frameworkless source root owns the loose file");
    assert!(source.files.iter().any(|file| file.ends_with("Loose.sol")));
    assert!(!source.files.iter().any(|file| file.ends_with("P.sol")));
}

/// `fs/=lib/fs/src/` resolves to the `lib/fs` package root — the same
/// root the library scan produced, so the graph deduplicates it.
#[test]
fn remapping_into_a_source_dir_resolves_to_the_package_root() {
    let root = fixture_path("fixtures/workspaces/foundry/nested");
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
        .roots
        .iter()
        .find(|source| source.is_dependency && source.path.ends_with("dependencies/fs"))
        .expect("dependencies/fs is a dependency source root");
    assert!(
        dependency
            .files
            .iter()
            .any(|file| file.ends_with("Lib.sol"))
    );
}

#[test]
fn remapping_into_an_ignored_library_copy_stays_ignored() {
    let root = fixture_path("fixtures/workspaces/foundry/nested");
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
    assert!(
        !discovered
            .roots
            .iter()
            .any(|source| { source.path.ends_with("dep-a/lib/duplicate") })
    );
}

/// A dependency's `test`/`script` dirs are outside the compilation unit:
/// they land on the ignored list and are pruned even when the dep has no
/// source dirs and indexes from its package root.
#[test]
fn dependency_dev_dirs_are_ignored_and_not_indexed() {
    let root = fixture_path("fixtures/workspaces/foundry/dev_dirs");
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
        .roots
        .iter()
        .find(|source| source.path.ends_with("lib/dep"))
        .expect("the dep indexes from its package root");
    assert!(dep.is_dependency);
    assert!(dep.files.iter().any(|file| file.ends_with("Dep.sol")));
    assert!(!dep.files.iter().any(|file| file.ends_with("Dep.t.sol")));
    assert!(!dep.files.iter().any(|file| file.ends_with("Deploy.s.sol")));
}

/// A package is identified by its root directory, not by its position in the
/// package list — so adding a package cannot renumber a surviving root.
#[test]
fn a_source_root_names_its_package_by_path() {
    let root = fixture_path("fixtures/workspaces/foundry/nested");
    let discovered = discover_workspace(&root);

    assert!(
        discovered
            .roots
            .iter()
            .any(|source| source.path != source.package_id.0),
        "a foundry package's source root is its src dir, not its package root"
    );
    for source in &discovered.roots {
        assert!(
            discovered
                .packages
                .iter()
                .any(|package| package.root == source.package_id.0),
            "{} names a package that is not in the workspace",
            source.path
        );
    }
}
/// A dependency can ship a manifest inside its own source dir: npm does it for
/// `@openzeppelin/contracts`, whose `contracts/package.json` is the published
/// package root. A remapping into that dir (`@openzeppelin/contracts/=contracts/`)
/// is an alias, not an edge to another package — treating it as one indexed the
/// same files twice, and the copy took ownership of them, dropping the parent's
/// remappings from their scope.
#[test]
fn a_remapping_into_the_packages_own_sources_is_not_a_package() {
    let root = fixture_path("fixtures/workspaces/foundry/self_remap");
    let discovered = discover_workspace(&root);

    let package_roots = discovered
        .packages
        .iter()
        .map(|package| package.root.clone())
        .collect::<Vec<_>>();
    assert!(
        !package_roots
            .iter()
            .any(|package| package.ends_with("lib/dep/contracts")),
        "the source dir is not a package of its own: {package_roots:?}"
    );

    let roots = discovered
        .roots
        .iter()
        .filter(|source| source.path.ends_with("lib/dep/contracts"))
        .collect::<Vec<_>>();
    assert_eq!(roots.len(), 1, "one root indexes those files");
    assert_eq!(
        roots[0].package_id.0,
        root.join("lib/dep"),
        "the files belong to the package that declares the source dir"
    );

    // Dropping the edge must not drop the alias: imports still resolve.
    let dep = discovered
        .packages
        .iter()
        .find(|package| package.root.ends_with("lib/dep"))
        .expect("dep is a package");
    assert!(
        dep.remappings.iter().any(|remapping| {
            remapping.prefix == "@dep/" && remapping.target.ends_with("lib/dep/contracts")
        }),
        "the remapping survives: {:?}",
        dep.remappings
    );
}
