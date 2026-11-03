use crate::workspace::Remapping;
use crate::workspace::layout::{merge_remappings, owner_remappings};
use camino::{Utf8Path, Utf8PathBuf};

#[test]
fn root_remappings_override_dependency_duplicates_and_keep_dependency_only_prefixes() {
    let root_mapping = Remapping {
        context: None,
        prefix: "@openzeppelin/contracts/".to_string(),
        target: Utf8PathBuf::from("/root/lib/openzeppelin-contracts/contracts"),
    };
    let dependency_mapping = Remapping {
        context: None,
        prefix: "@openzeppelin/contracts/".to_string(),
        target: Utf8PathBuf::from("/root/lib/upgradeable/lib/openzeppelin-contracts/contracts"),
    };
    let dependency_only = Remapping {
        context: None,
        prefix: "@vendor/".to_string(),
        target: Utf8PathBuf::from("/root/lib/upgradeable/lib/vendor/src"),
    };

    let merged = merge_remappings(
        std::slice::from_ref(&root_mapping),
        vec![dependency_mapping, dependency_only.clone()],
    );

    assert_eq!(merged.len(), 2);
    assert!(merged.contains(&root_mapping));
    assert!(merged.contains(&dependency_only));
}

#[test]
fn dependency_remappings_come_from_the_nearest_enclosing_package() {
    let remapping = |prefix: &str| Remapping {
        context: None,
        prefix: prefix.to_string(),
        target: Utf8PathBuf::from("/mono/lib/target"),
    };
    let owners = vec![
        (Utf8PathBuf::from("/mono"), vec![remapping("root/")]),
        (Utf8PathBuf::from("/mono/packages/a"), vec![remapping("a/")]),
        (Utf8PathBuf::from("/mono/packages/b"), vec![remapping("b/")]),
    ];
    let fallback = vec![remapping("fallback/")];

    let a = owner_remappings(Utf8Path::new("/mono/packages/a/lib/x"), &owners, &fallback);
    let b = owner_remappings(Utf8Path::new("/mono/packages/b/lib/x"), &owners, &fallback);
    let hoisted = owner_remappings(Utf8Path::new("/elsewhere/node_modules/x"), &owners, &fallback);

    assert_eq!(a[0].prefix, "a/");
    assert_eq!(b[0].prefix, "b/");
    assert_eq!(hoisted[0].prefix, "fallback/");
}
