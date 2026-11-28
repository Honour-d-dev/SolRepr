use camino::Utf8Path;
use rustc_hash::FxHashMap;
use smol_str::SmolStr;

use crate::salsa::FileId;

/// The imports a file declares, resolved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Imports {
    pub resolved: Vec<FileId>,
    pub unresolved: Vec<SmolStr>,
}

/// The inversion of [`Imports::resolved`]: for each file, the files importing/depending on it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportMap(FxHashMap<FileId, Vec<FileId>>);

impl ImportMap {
    pub fn insert(&mut self, target: FileId, importer: FileId) {
        self.0.entry(target).or_default().push(importer);
    }

    /// The files importing `target` directly.
    pub fn of(&self, target: FileId) -> &[FileId] {
        self.0.get(&target).map_or(&[], Vec::as_slice)
    }
}

/// A file and the source-unit name it is compiled under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceUnit {
    pub name: Box<str>,
    pub file: FileId,
}

/// The name solc assigns `path` when `base` is its base path.
///
/// A path under `base` is named relative to it, so a name is a property of the
/// base path as much as of the file. A path outside `base` keeps its absolute
/// spelling, which is what solc does for one.
pub fn source_unit_name(base: &Utf8Path, path: &Utf8Path) -> Box<str> {
    path.strip_prefix(base)
        .unwrap_or(path)
        .as_str()
        .replace(std::path::MAIN_SEPARATOR, "/")
        .into()
}
