use std::time::Duration;

use camino::Utf8PathBuf;
use crossbeam_channel::{Receiver, Sender};

use crate::worker::Job;

use super::diff::{WorkspaceDiff, diff};
use super::{LoadedFile, Workspace, WorkspaceWithFiles, discover_workspaces};

/// A burst of writes — `forge install`, `npm ci`, a checkout — should produce
/// one discovery, not one per event.
const DEBOUNCE: Duration = Duration::from_millis(500);

/// A request to rediscover the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReloadRequest {
    /// Something on disk may have moved; rediscover the folders the worker
    /// already holds.
    Rediscover,
    /// The client's workspace folder set moved. The delta is applied to the folders the worker holds
    Folders {
        added: Box<[Utf8PathBuf]>,
        removed: Box<[Utf8PathBuf]>,
    },
}

impl ReloadRequest {
    /// How two requests that arrived in one burst collapse into one.
    fn combine(self, newer: Self) -> Self {
        match (self, newer) {
            (
                Self::Folders { added, removed },
                Self::Folders {
                    added: added_newer,
                    removed: removed_newer,
                },
            ) => Self::Folders {
                added: union(added, added_newer),
                removed: union(removed, removed_newer),
            },
            (folders @ Self::Folders { .. }, Self::Rediscover) => folders,
            (Self::Rediscover, newer) => newer,
        }
    }
}

/// Both paths, each named once, in arrival order.
fn union(first: Box<[Utf8PathBuf]>, second: Box<[Utf8PathBuf]>) -> Box<[Utf8PathBuf]> {
    let mut paths = first.into_vec();
    for path in second.into_vec() {
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths.into_boxed_slice()
}

/// Holds the last workspace the db saw and the folders to discover, and sends a
/// diff only when the two disagree.
pub(crate) struct WorkspaceWorker {
    /// The folders this session loads. The worker owns them, so a folder
    /// change is a delta applied here rather than a list sent from the loop.
    roots: Vec<Utf8PathBuf>,
    workspace: Workspace<()>,
}

impl WorkspaceWorker {

    pub(crate) fn new(roots: Box<[Utf8PathBuf]>, workspace: Workspace<()>) -> Self {
        Self { roots: roots.into_vec(), workspace }
    }

    /// Apply a folder delta to the folders the worker holds.
    fn apply(&mut self, request: ReloadRequest) {
        let ReloadRequest::Folders { added, removed } = request else {
            return;
        };
        for root in added.into_vec() {
            if !self.roots.contains(&root) {
                self.roots.push(root);
            }
        }
        self.roots.retain(|root| !removed.contains(root));
    }

    /// Compare a discovery against the root set the last diff left behind.
    fn plan(&self, new: WorkspaceWithFiles<Utf8PathBuf>) -> (Workspace<()>, WorkspaceDiff<LoadedFile>) {
        let (next, diff) = diff(&self.workspace, new);
        // Converting to `LoadedFile` reads every added file from disk.
        (next, WorkspaceDiff::<LoadedFile>::from(diff))
    }
}

impl Job for WorkspaceWorker {
    type Req = ReloadRequest;
    type Res = WorkspaceDiff<LoadedFile>;

    fn run(mut self, rx: Receiver<Self::Req>, tx: Sender<Self::Res>) {
        let mut pending = None;

        while let Ok(request) = match pending.take() {
            Some(request) => Ok(request),
            None => rx.recv(),
        } {
            self.apply(Self::debounce(&rx, request, DEBOUNCE, ReloadRequest::combine));

            let (next, diff) = self.plan(discover_workspaces(&self.roots));

            // A newer request arrived while discovery ran: this pass is already
            // stale. Drop it without moving `workspace`, and debounce again
            match Self::coalesce(&rx, ReloadRequest::combine) {
                Some(newer) => {
                    pending = Some(newer);
                    continue;
                }
                // Nothing moved. The loop has nothing to apply, and no request
                // is waiting on a reply, so the pass can end here.
                _ if diff.is_empty() => continue,
                // Advance only on send: the next diff is computed against what
                // the loop actually applied.
                _ => {                    self.workspace = next;
                    if tx.send(diff).is_err() {
                        return;
                    }
                }
            }
        }
    }
}
