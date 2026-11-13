//! Pure parsers for the config formats discovery reads. Each has a `*_text`
//! entry point, so parsing is tested without touching the filesystem.

use std::fs;

use camino::Utf8Path;

use super::Remapping;
use crate::utilities::normalize_path;

/// The package.json data needed to expand the package graph. Deliberately
/// small: plugin execution is outside workspace discovery.
#[derive(Debug, Clone, Default)]
pub(super) struct PackageManifest {
    pub workspace_patterns: Vec<String>,
    pub dependency_names: Vec<String>,
}

pub(super) fn parse_package_manifest(path: &Utf8Path) -> Option<PackageManifest> {
    let text = fs::read_to_string(path).ok()?;
    parse_package_manifest_text(&text)
}

pub(super) fn parse_package_manifest_text(text: &str) -> Option<PackageManifest> {
    let value = serde_json::from_str::<serde_json::Value>(text).ok()?;
    let mut workspace_patterns = match value.get("workspaces") {
        Some(serde_json::Value::Array(workspaces)) => workspaces
            .iter()
            .filter_map(|pattern| pattern.as_str().map(String::from))
            .collect(),
        Some(serde_json::Value::Object(workspaces)) => workspaces
            .get("packages")
            .and_then(serde_json::Value::as_array)
            .map(|packages| {
                packages
                    .iter()
                    .filter_map(|pattern| pattern.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    let mut dependency_names = [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ]
    .into_iter()
    .filter_map(|key| value.get(key)?.as_object())
    .flat_map(|dependencies| dependencies.keys().cloned())
    .collect::<Vec<_>>();
    workspace_patterns.sort();
    workspace_patterns.dedup();
    dependency_names.sort();
    dependency_names.dedup();
    Some(PackageManifest {
        workspace_patterns,
        dependency_names,
    })
}

/// The `hardhat.config.*` values discovery needs: which directories are
/// source roots, which hold tests, and which hold vendored dependencies
/// (`imports`). Defaults match Hardhat.
#[derive(Debug, Clone)]
pub(super) struct HardhatConfig {
    pub sources: Vec<String>,
    pub tests: Vec<String>,
    pub imports: Vec<String>,
}

impl Default for HardhatConfig {
    fn default() -> Self {
        Self {
            sources: vec!["contracts".to_string()],
            tests: vec!["test".to_string()],
            imports: vec!["imports".to_string()],
        }
    }
}

pub(super) fn parse_hardhat_config(path: &Utf8Path) -> HardhatConfig {
    fs::read_to_string(path)
        .ok()
        .map(|text| parse_hardhat_config_text(&text))
        .unwrap_or_default()
}

/// Extract `paths` from a Hardhat config. The file is text, not JavaScript, so
/// computed or `require`d values fall back to the defaults; keys match on
/// identifier boundaries so `dataSources` cannot leak in.
pub(super) fn parse_hardhat_config_text(text: &str) -> HardhatConfig {
    let text = strip_comments(text);
    let Some(paths) = paths_block(&text) else {
        return HardhatConfig::default();
    };
    HardhatConfig {
        sources: string_list(paths, "sources")
            .unwrap_or_else(|| vec!["contracts".to_string()]),
        tests: string_list(paths, "tests").unwrap_or_else(|| vec!["test".to_string()]),
        imports: string_list(paths, "imports")
            .unwrap_or_else(|| vec!["imports".to_string()]),
    }
}

/// `//` and `/* */` comments removed from a JavaScript/TypeScript config.
/// Quoted strings are preserved verbatim so a `//` inside a literal does not
/// start a comment.
fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                out.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' | '`' => {
                    quote = Some(c);
                    out.push(c);
                }
                '/' if chars.peek() == Some(&'/') => {
                    for c in chars.by_ref() {
                        if c == '\n' {
                            out.push('\n');
                            break;
                        }
                    }
                }
                '/' if chars.peek() == Some(&'*') => {
                    chars.next();
                    let mut star = false;
                    for c in chars.by_ref() {
                        if star && c == '/' {
                            break;
                        }
                        star = c == '*';
                    }
                }
                _ => out.push(c),
            },
        }
    }
    out
}

/// The contents of `paths { ... }`, or `None` when the config has no paths
/// block.
fn paths_block(text: &str) -> Option<&str> {
    let start = find_identifier(text, "paths")?;
    let after_key = text[start + "paths".len()..].trim_start();
    let open = after_key.find('{')?;
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    for (i, c) in after_key[open..].char_indices() {
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"' | '`') => quote = Some(c),
            (None, '{') => depth += 1,
            (None, '}') => {
                depth -= 1;
                if depth == 0 {
                    return Some(&after_key[open + 1..open + i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// The index of `key` used as a property key (`key:` / `key =`). An
/// identifier boundary on the left keeps `dataSources` from matching
/// `sources`.
fn find_identifier(text: &str, key: &str) -> Option<usize> {
    text.match_indices(key)
        .find(|(index, _)| {
            let left_boundary = text[..*index]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric() && c != '_' && c != '$');
            let is_key = text[index + key.len()..]
                .trim_start()
                .starts_with([':', '=']);
            left_boundary && is_key
        })
        .map(|(index, _)| index)
}

/// The string or string-array value of `key` inside a `paths { }` block.
fn string_list(paths: &str, key: &str) -> Option<Vec<String>> {
    let index = find_identifier(paths, key)?;
    let value = paths[index + key.len()..].trim_start();
    let value = value.strip_prefix(':').or_else(|| value.strip_prefix('='))?;
    let value = value.trim_start();

    let values = if let Some(list) = value.strip_prefix('[') {
        let end = list.find(']')?;
        quoted_strings(&list[..end])
    } else {
        quoted_strings(value).into_iter().take(1).collect()
    };
    (!values.is_empty()).then_some(values)
}

/// Every quoted string in `text`, in order.
fn quoted_strings(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut quote: Option<char> = None;
    let mut start = 0;
    for (i, c) in text.char_indices() {
        match (quote, c) {
            (Some(q), _) if c == q => {
                quote = None;
                let value = text[start + q.len_utf8()..i].trim();
                if !value.is_empty() {
                    out.push(value.to_string());
                }
            }
            (Some(_), _) => {}
            (None, '\'' | '"' | '`') => {
                quote = Some(c);
                start = i;
            }
            _ => {}
        }
    }
    out
}

/// The `foundry.toml` facts discovery needs: source dir, library dirs and
/// remappings. `libs` defaults to `["lib"]`, matching forge.
#[derive(Debug, Clone)]
pub(super) struct FoundryConfig {
    pub src: String,
    pub test: String,
    pub script: String,
    pub libs: Vec<String>,
    pub auto_detect_remappings: bool,
    pub remappings: Vec<Remapping>,
}

impl Default for FoundryConfig {
    fn default() -> Self {
        Self {
            src: "src".to_string(),
            test: "test".to_string(),
            script: "script".to_string(),
            libs: vec!["lib".to_string()],
            auto_detect_remappings: true,
            remappings: Vec::new(),
        }
    }
}

/// Extracts the Foundry configuration from foundry.toml.
/// Handles [profile.default], [default] (backwards-compat), and top-level keys.
pub(super) fn parse_foundry_toml(text: &str, package_root: &Utf8Path) -> Option<FoundryConfig> {
    let doc = toml::from_str::<toml::Value>(text).ok()?;

    // [profile.default] or backwards-compat [default]; otherwise top-level keys.
    let section = doc
        .get("profile")
        .and_then(|profile| profile.get("default"))
        .or_else(|| doc.get("default"))
        .unwrap_or(&doc);

    let string_key = |key: &str, default: &str| {
        section
            .get(key)
            .and_then(|value| value.as_str())
            .unwrap_or(default)
            .to_string()
    };
    let src = string_key("src", "src");
    let test = string_key("test", "test");
    let script = string_key("script", "script");
    let libs = section
        .get("libs")
        .and_then(|value| value.as_array())
        .map(|libs| {
            libs.iter()
                .filter_map(|lib| lib.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_else(|| vec!["lib".to_string()]);
    let auto_detect_remappings = section
        .get("auto_detect_remappings")
        .and_then(|value| value.as_bool())
        .unwrap_or(true);
    let remappings = section
        .get("remappings")
        .and_then(|value| value.as_array())
        .map(|remappings| parse_remap_array(remappings, package_root))
        .unwrap_or_default();

    Some(FoundryConfig {
        src,
        test,
        script,
        libs,
        auto_detect_remappings,
        remappings,
    })
}

fn parse_remap_array(arr: &[toml::Value], package_root: &Utf8Path) -> Vec<Remapping> {
    arr.iter()
        .filter_map(toml::Value::as_str)
        .filter_map(|entry| parse_remapping_entry(package_root, entry))
        .collect()
}

pub(super) fn parse_remappings_txt(path: &Utf8Path) -> Option<Vec<Remapping>> {
    let text = fs::read_to_string(path).ok()?;
    let package_root = path.parent()?;
    Some(
        text.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .filter_map(|entry| parse_remapping_entry(package_root, entry))
            .collect(),
    )
}

pub(super) fn parse_remapping_entry(package_root: &Utf8Path, entry: &str) -> Option<Remapping> {
    let (lhs, target) = entry.trim().split_once('=')?;
    let (context, prefix) = match lhs.trim().split_once(':') {
        Some((context, prefix)) if !context.trim().is_empty() => (
            Some(normalize_path(&package_root.join(context.trim()))),
            prefix.trim(),
        ),
        _ => (None, lhs.trim()),
    };
    let target = target.trim();
    if prefix.is_empty() || target.is_empty() {
        return None;
    }
    let target = Utf8Path::new(target);
    let target = if target.is_absolute() {
        normalize_path(target)
    } else {
        normalize_path(&package_root.join(target))
    };
    Some(Remapping {
        context,
        prefix: prefix.to_string(),
        target,
    })
}

