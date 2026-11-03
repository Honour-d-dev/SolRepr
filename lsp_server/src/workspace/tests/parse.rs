use crate::workspace::parse::{
    parse_foundry_toml, parse_hardhat_config_text, parse_package_manifest_text,
    parse_remapping_entry,
};
use camino::{Utf8Path, Utf8PathBuf};

#[test]
fn parses_foundry_libs_and_auto_detect_flag() {
    let config = parse_foundry_toml(
        r#"
        [profile.default]
        src = "contracts"
        libs = ["dependencies", "node_modules"]
        auto_detect_remappings = false
        "#,
        Utf8Path::new("/root"),
    )
    .unwrap();

    assert_eq!(config.src, "contracts");
    assert_eq!(config.libs, vec!["dependencies", "node_modules"]);
    assert!(!config.auto_detect_remappings);

    // Missing keys fall back to forge defaults.
    let default = parse_foundry_toml("", Utf8Path::new("/root")).unwrap();
    assert_eq!(default.src, "src");
    assert_eq!(default.libs, vec!["lib"]);
    assert!(default.auto_detect_remappings);

    // Backwards-compat [default] section carries src + remappings.
    let compat = parse_foundry_toml(
        r#"
        [default]
        src = "code"
        remappings = ["forge-std/=lib/forge-std/src"]
        "#,
        Utf8Path::new("/root"),
    )
    .unwrap();
    assert_eq!(compat.src, "code");
    assert_eq!(compat.remappings[0].prefix, "forge-std/");
    assert!(
        compat.remappings[0]
            .target
            .ends_with("lib/forge-std/src")
    );

    // Top-level keys without any section still apply.
    let top_level = parse_foundry_toml("src = \"raw\"", Utf8Path::new("/root")).unwrap();
    assert_eq!(top_level.src, "raw");
}

#[test]
fn parses_context_and_normalizes_remapping_paths() {
    let remapping = parse_remapping_entry(
        Utf8Path::new("/root"),
        "lib/upgradeable/:@openzeppelin/contracts/=lib/openzeppelin-contracts/contracts",
    )
    .unwrap();

    assert_eq!(
        remapping.context,
        Some(Utf8PathBuf::from("/root/lib/upgradeable"))
    );
    assert_eq!(remapping.prefix, "@openzeppelin/contracts/");
    assert_eq!(
        remapping.target,
        Utf8PathBuf::from("/root/lib/openzeppelin-contracts/contracts")
    );
}

#[test]
fn parses_hardhat_paths_block() {
    let config = parse_hardhat_config_text(
        r#"
        // paths: { sources: "./not-this" }
        module.exports = {
            url: "https://example.com//still-a-string",
            paths: {
                sources: ["./src", "./contracts"],
                tests: './test',
                imports: "./vendor"
            },
            other: { sources: "./not-this-either" }
        };
        "#,
    );

    assert_eq!(config.sources, vec!["./src", "./contracts"]);
    assert_eq!(config.tests, vec!["./test"]);
    assert_eq!(config.imports, vec!["./vendor"]);
}

#[test]
fn hardhat_config_without_paths_block_uses_defaults() {
    let config = parse_hardhat_config_text("module.exports = {};");
    assert_eq!(config.sources, vec!["contracts"]);
    assert_eq!(config.tests, vec!["test"]);
    assert_eq!(config.imports, vec!["imports"]);
}

#[test]
fn parses_workspace_and_dependency_references_from_one_manifest() {
    let manifest = parse_package_manifest_text(
        r#"
        {
            "workspaces": { "packages": ["packages/*"] },
            "dependencies": { "ethers": "^6.0.0", "solmate": "^6.0.0" },
            "devDependencies": { "hardhat": "^3.0.0" },
            "peerDependencies": { "viem": "^2.0.0" }
        }
    "#,
    )
    .unwrap();

    assert_eq!(manifest.workspace_patterns, vec!["packages/*"]);
    assert_eq!(
        manifest.dependency_names,
        vec!["ethers", "hardhat", "solmate", "viem"]
    );
}
