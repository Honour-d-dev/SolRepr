use std::borrow::Borrow;
use std::fmt;
use std::ops::Deref;
use std::path::Path;

use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};

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

pub(crate) fn canonicalize_path(path: &Utf8Path) -> anyhow::Result<Utf8PathBuf> {
    let path = dunce::canonicalize(path).context("failed to canonicalize path")?;
    Utf8PathBuf::from_path_buf(path).map_err(|path| anyhow::anyhow!("path is not valid UTF-8: {path:?}"))
}

pub(crate) fn normalize_path(path: &Utf8Path) -> Utf8PathBuf {
    if let Ok(path) = canonicalize_path(path) {
        return path;
    }

    // Canonicalizing can fail when the file has not been saved to disk.
    let Some(parent) = path.parent() else {
        return path.to_path_buf();
    };
    let Ok(normalized_parent) = canonicalize_path(parent) else {
        return path.to_path_buf();
    };
    normalized_parent.join(path.file_name().unwrap_or_default())
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
}
