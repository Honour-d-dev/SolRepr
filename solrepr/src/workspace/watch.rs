use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use crossbeam_channel::Sender;
use lsp_server::{Message, Request};
use lsp_types::{
    DidChangeWatchedFilesClientCapabilities, DidChangeWatchedFilesRegistrationOptions, FileEvent,
    FileSystemWatcher, GlobPattern, InitializeParams, OneOf, Registration, RegistrationParams,
    RelativePattern, Unregistration, UnregistrationParams,
    notification::{DidChangeWatchedFiles, Notification as _},
    request::{RegisterCapability, Request as _, UnregisterCapability},
};
use rustc_hash::FxHashSet;

use super::is_manifest;
use crate::{salsa::File, utilities::{to_url, to_utf8path}};

/// The globs the server asks the client to watch. A manifest can add or remove
/// roots; a `.sol` file is one path in or out of an existing root.
const WATCHED_FILES: [&str; 5] = [
    "**/foundry.toml",
    "**/remappings.txt",
    "**/package.json",
    "**/hardhat.config.*",
    "**/*.sol",
];

/// The id the watched-files registration is made under, so it can be revoked.
const WATCHED_FILES_REGISTRATION: &str = "solrepr-watched-files";

/// A watched path the server cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Watched {
    /// The contents decide the shape of a package: discovery has to run again.
    Manifest,
    /// One path in or out of an existing root.
    Source,
}

/// What a watcher event did to a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeEvent {
    /// Nothing to do: an open document, an ignored root, or a path that is
    /// neither indexed nor on disk.
    Skipped,
    /// A known path whose text changed on disk.
    Updated(File),
    /// A known path that vanished, so imports of it are unresolved again.
    Removed(File),
    /// A new path under a known root.
    Created(File),
    /// On disk but under no root we know: discovery may need to run.
    Unrooted,
}

/// The paths a batch names, deduplicated.
///
/// A burst can name one path twice — two watchers, or a create then a change.
pub(crate) fn changed_paths(changes: &[FileEvent]) -> Vec<Utf8PathBuf> {
    let mut seen = FxHashSet::default();
    changes
        .iter()
        .filter_map(|event| to_utf8path(&event.uri).ok())
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

/// The kind of path a watcher event named, or `None` when it cannot change
/// anything the server holds.
pub(crate) fn classify(path: &Utf8Path) -> Option<Watched> {
    if is_manifest(path) {
        return Some(Watched::Manifest);
    }
    (path.extension() == Some("sol")).then_some(Watched::Source)
}


/// The client's watched-files capabilities, when it supports dynamic
/// registration at all.
fn capabilities(params: &InitializeParams) -> Option<&DidChangeWatchedFilesClientCapabilities> {
    params
        .capabilities
        .workspace
        .as_ref()
        .and_then(|workspace| workspace.did_change_watched_files.as_ref())
        .filter(|capabilities| capabilities.dynamic_registration == Some(true))
}

/// Whether the watchers are registered per folder.
///
/// Relative patterns are folder-scoped, so the folder set is the registration
/// set. Without them one folder-independent registration covers everything.
pub(crate) fn registers_per_folder(params: &InitializeParams) -> bool {
    capabilities(params).is_some_and(|capabilities| capabilities.relative_pattern_support == Some(true))
}

/// The registration id for one folder, derived from its path so it can be
/// revoked without a map.
fn watch_registration(root: &Utf8Path) -> String {
    format!("{WATCHED_FILES_REGISTRATION}:{root}")
}

fn watcher(glob_pattern: GlobPattern) -> FileSystemWatcher {
    FileSystemWatcher {
        glob_pattern,
        // Create, change and delete: the handler tells them apart.
        kind: None,
    }
}

/// The globs, scoped to one folder.
fn folder_watchers(root: &Utf8Path) -> Vec<FileSystemWatcher> {
    let base_uri = OneOf::Right(to_url(root));
    WATCHED_FILES
        .iter()
        .map(|pattern| {
            watcher(GlobPattern::Relative(RelativePattern {
                base_uri: base_uri.clone(),
                pattern: (*pattern).to_string(),
            }))
        })
        .collect()
}

/// The globs, matching anywhere.
fn plain_watchers() -> Vec<FileSystemWatcher> {
    WATCHED_FILES
        .iter()
        .map(|pattern| watcher(GlobPattern::String((*pattern).to_string())))
        .collect()
}

/// Ask the client to watch the globs the server cares about, so no client has
/// to hand-roll the list.
pub(crate) fn register_watchers(
    sender: &Sender<Message>,
    params: &InitializeParams,
    roots: &[Utf8PathBuf],
) -> anyhow::Result<()> {
    let Some(capabilities) = capabilities(params) else {
        return Ok(());
    };

    if capabilities.relative_pattern_support == Some(true) {
        for root in roots {
            register_folder(sender, root)?;
        }
        Ok(())
    } else {
        register(sender, WATCHED_FILES_REGISTRATION.to_string(), plain_watchers())
    }
}

/// Watch one folder. The id is the folder's, so it can be revoked on its own.
pub(crate) fn register_folder(sender: &Sender<Message>, root: &Utf8Path) -> anyhow::Result<()> {
    register(sender, watch_registration(root), folder_watchers(root))
}

/// Stop watching one folder.
pub(crate) fn unregister_folder(sender: &Sender<Message>, root: &Utf8Path) -> anyhow::Result<()> {
    let id = watch_registration(root);
    let params = UnregistrationParams {
        unregisterations: vec![Unregistration {
            id: id.clone(),
            method: DidChangeWatchedFiles::METHOD.to_string(),
        }],
    };
    request(sender, id, UnregisterCapability::METHOD, params)
}

fn register(sender: &Sender<Message>, id: String, watchers: Vec<FileSystemWatcher>) -> anyhow::Result<()> {
    let options = DidChangeWatchedFilesRegistrationOptions { watchers };
    let params = RegistrationParams {
        registrations: vec![Registration {
            id: id.clone(),
            method: DidChangeWatchedFiles::METHOD.to_string(),
            register_options: Some(serde_json::to_value(options)?),
        }],
    };
    request(sender, id, RegisterCapability::METHOD, params)
}

/// Send a `client/*Capability` request.
///
/// The request id is the registration id: unique per folder, and named in a log.
fn request(
    sender: &Sender<Message>,
    id: String,
    method: &'static str,
    params: impl serde::Serialize,
) -> anyhow::Result<()> {
    let params = serde_json::to_value(params)?;
    sender
        .send(Message::Request(Request::new(id.into(), method.to_string(), params)))
        .context("the client connection is gone")?;
    Ok(())
}
