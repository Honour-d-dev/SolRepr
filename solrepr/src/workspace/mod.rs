//! Workspace discovery: directories in, a package graph and its source roots
//! out. `detect` reads manifests, `graph` walks dependencies, `loader` reads,
//! `model` is the shape they produce, `watch`/`worker` keep it current.

mod detect;
mod discover;
pub(crate) mod diff;
mod graph;
mod layout;
pub(crate) mod loader;
mod ws_types;
mod parse;
pub(crate) mod watch;
pub(crate) mod worker;

#[cfg(test)]
mod tests;

use anyhow::Context;
use camino::Utf8PathBuf;
use lsp_types::InitializeParams;

use crate::utilities::to_utf8path;

use discover::discover_workspace;

pub(crate) use detect::is_manifest;
pub(crate) use loader::LoadedFile;
pub use ws_types::{Package, PackageId, Remapping};
pub(crate) use ws_types::{Root, RootWithFiles, Workspace, WorkspaceWithFiles};

/// The directories this session loads: every `workspaceFolders` entry, else
/// `rootUri`, else the working directory. A folder the server cannot read is
/// skipped rather than failing the handshake.
pub(crate) fn workspace_roots(params: &InitializeParams) -> anyhow::Result<Box<[Utf8PathBuf]>> {
    let mut roots: Vec<Utf8PathBuf> = Vec::new();
    for folder in params.workspace_folders.as_deref().unwrap_or_default() {
        if let Ok(path) = to_utf8path(&folder.uri) && !roots.contains(&path) {
            roots.push(path);
        }
    }

    if roots.is_empty() {
        match params.root_uri {
            Some(ref uri) => roots.push(to_utf8path(uri)?),
            None => {
                let cwd = std::env::current_dir().context("no workspace root and no working directory")?;
                roots.push(Utf8PathBuf::from_path_buf(cwd).map_err(|path| anyhow::anyhow!("working directory is not valid UTF-8: {path:?}"))?);
            }
        }
    }

    Ok(roots.into_boxed_slice())
}

/// Discover each folder and merge the results. One folder skips the merge,
/// since it has nothing to overlap with.
pub(crate) fn discover_workspaces(roots: &[Utf8PathBuf]) -> WorkspaceWithFiles<Utf8PathBuf> {
    match roots {
        [single] => discover_workspace(single),
        multi => WorkspaceWithFiles::merge(multi.iter().map(|root| discover_workspace(root))),
    }
}
