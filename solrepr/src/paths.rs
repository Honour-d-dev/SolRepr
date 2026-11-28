use std::borrow::Borrow;
use std::fmt;
use std::ops::Deref;
use std::path::Path;

use anyhow::Context;
use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use lsp_types::Url;

/// A filesystem path normalized by [`normalize_path`].
///
/// The field is private and the unchecked constructor is discovery-only, so
/// paths stored in workspace identity types cannot be unnormalized by accident.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NormalizedPath(Utf8PathBuf);

impl NormalizedPath {
    pub fn new(path: impl AsRef<Utf8Path>) -> Self {
        Self(normalize_path(path.as_ref()))
    }

    /// Trusted constructor for discovery walk entries beneath a root that
    /// discovery has already canonicalized. Performs no I/O and no checks.
    pub(crate) fn new_unchecked(path: Utf8PathBuf) -> Self {
        Self(path)
    }

    pub fn as_path(&self) -> &Utf8Path {
        &self.0
    }

    pub fn into_inner(self) -> Utf8PathBuf {
        self.0
    }

    pub fn to_url(&self) -> Url {
        Url::from_file_path(self.as_path()).expect("failed to convert path to url")
    }
}

impl TryFrom<&Url> for NormalizedPath {
    type Error = anyhow::Error;

    fn try_from(uri: &Url) -> Result<Self, Self::Error> {
        let path = uri.to_file_path().map_err(|_| anyhow::anyhow!("failed to convert URI to path"))?;
        let path = Utf8PathBuf::from_path_buf(path).map_err(|_| anyhow::anyhow!("path is not valid UTF-8"))?;
        Ok(Self::new(path))
    }
}

impl Deref for NormalizedPath {
    type Target = Utf8Path;

    fn deref(&self) -> &Utf8Path {
        &self.0
    }
}

impl AsRef<Utf8Path> for NormalizedPath {
    fn as_ref(&self) -> &Utf8Path {
        &self.0
    }
}

impl AsRef<Path> for NormalizedPath {
    fn as_ref(&self) -> &Path {
        self.0.as_std_path()
    }
}

impl Borrow<Utf8Path> for NormalizedPath {
    fn borrow(&self) -> &Utf8Path {
        &self.0
    }
}

impl fmt::Debug for NormalizedPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl fmt::Display for NormalizedPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl PartialEq<Utf8Path> for NormalizedPath {
    fn eq(&self, other: &Utf8Path) -> bool {
        self.0 == other
    }
}

impl PartialEq<Utf8PathBuf> for NormalizedPath {
    fn eq(&self, other: &Utf8PathBuf) -> bool {
        self.0 == *other
    }
}

impl PartialEq<NormalizedPath> for Utf8PathBuf {
    fn eq(&self, other: &NormalizedPath) -> bool {
        *self == other.0
    }
}

impl From<Utf8PathBuf> for NormalizedPath {
    fn from(path: Utf8PathBuf) -> Self {
        Self::new(path)
    }
}

impl From<&Utf8Path> for NormalizedPath {
    fn from(path: &Utf8Path) -> Self {
        Self::new(path)
    }
}

impl From<&Utf8PathBuf> for NormalizedPath {
    fn from(path: &Utf8PathBuf) -> Self {
        Self::new(path)
    }
}

impl From<&NormalizedPath> for NormalizedPath {
    fn from(path: &NormalizedPath) -> Self {
        path.clone()
    }
}

impl From<NormalizedPath> for Utf8PathBuf {
    fn from(path: NormalizedPath) -> Self {
        path.0
    }
}

fn canonicalize(path: &Utf8Path) -> anyhow::Result<Utf8PathBuf> {
    let path = dunce::canonicalize(path).context("failed to canonicalize path")?;
    Utf8PathBuf::from_path_buf(path).map_err(|path| anyhow::anyhow!("path is not valid UTF-8: {path:?}"))
}

fn lexicalize(path: &Utf8Path) -> Utf8PathBuf {
    let mut out = Utf8PathBuf::new();
    for component in path.components() {
        match component {
            Utf8Component::CurDir => {}
            Utf8Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Utf8Component::Normal(_))) {
                    out.pop();
                } else if !matches!(out.components().next_back(), Some(Utf8Component::RootDir)) {
                    out.push(component.as_str());
                }
            }
            other => out.push(other.as_str()),
        }
    }
    if out.as_str().is_empty() && !path.as_str().is_empty() {
        out.push(".");
    }
    out
}

pub(crate) fn normalize_path(path: &Utf8Path) -> Utf8PathBuf {
    if let Ok(path) = canonicalize(path) {
        return path;
    }

    // Canonicalizing can fail when the file has not been saved to disk.
    let lexical = lexicalize(path);
    if let Ok(path) = canonicalize(&lexical) {
        return path;
    }

    let mut tail = Vec::new();
    let mut ancestor = lexical.as_path();
    while let (Some(parent), Some(name)) = (ancestor.parent(), ancestor.file_name()) {
        tail.push(name);
        if let Ok(mut canonical) = canonicalize(parent) {
            for component in tail.into_iter().rev() {
                canonical.push(component);
            }
            return canonical;
        }
        ancestor = parent;
    }
    lexical
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_path_keeps_one_separator_style() {
        let path = format!("/solrepr_missing_{}/child.sol", std::process::id());
        let normalized = normalize_path(Utf8Path::new(&path));
        assert!(
            !(normalized.as_str().contains('/') && normalized.as_str().contains('\\')),
            "normalizing a missing path mixed separators: {normalized}"
        );
    }

    #[test]
    fn a_nested_missing_path_normalizes_its_existing_ancestor() {
        let temp_dir = Utf8PathBuf::from_path_buf(std::env::temp_dir()).expect("temp dir is UTF-8");
        let missing = format!("solrepr_missing_{}", std::process::id());
        let normalized = NormalizedPath::new(temp_dir.join(format!("{missing}/lib/dep/D.sol")));

        assert!(normalized.starts_with(NormalizedPath::new(temp_dir.join(&missing))));
        assert!(
            !(normalized.as_str().contains('/') && normalized.as_str().contains('\\')),
            "normalizing a missing path mixed separators: {normalized}"
        );
    }

    #[test]
    fn a_missing_path_collapses_parent_segments() {
        let temp_dir = Utf8PathBuf::from_path_buf(std::env::temp_dir()).expect("temp dir is UTF-8");
        let missing = format!("solrepr_missing_{}", std::process::id());

        assert_eq!(
            NormalizedPath::new(temp_dir.join(format!("{missing}/a/b/../c.sol"))),
            NormalizedPath::new(temp_dir.join(format!("{missing}/a/c.sol")))
        );
    }
}
