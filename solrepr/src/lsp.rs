use crate::capabilities::completion::completion;
use crate::capabilities::definition::definition;
use crate::capabilities::diagnostics::{DiagnosedFile, DiagnosticRequest, DiagnosticResult, DiagnosticsWorker};
use crate::capabilities::hover::hover;
use crate::capabilities::view_ast::{ViewAstRequest, view_ast};
use crate::capabilities::view_hir::{ViewHirRequest, view_hir};
use crate::live_docs::LiveDocs;
use crate::workspace::diff::WorkspaceDiff;
use crate::worker::Worker;
use crate::workspace::worker::WorkspaceWorker;
use crate::workspace::watch::{self, ChangeEvent, Watched};
use crate::workspace::worker::ReloadRequest;
use crate::workspace::{LoadedFile, discover_workspaces, workspace_roots};
use crate::salsa::{Db, File};
use crate::utilities::{document_path, log_info, to_url, to_utf8path};

use anyhow::Context;
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidChangeWatchedFiles, DidChangeWorkspaceFolders, DidCloseTextDocument,
    DidOpenTextDocument, Exit, Notification as LspNotification, PublishDiagnostics,
};
use lsp_types::request::{
    Completion as CompletionRequest, GotoDefinition, HoverRequest, Request as LspRequest, Shutdown,
};
use lsp_types::{
    CompletionOptions, Diagnostic as LspDiagnostic, DidChangeTextDocumentParams,
    DidChangeWatchedFilesParams, DidChangeWorkspaceFoldersParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, HoverProviderCapability, InitializeParams, InitializeResult, OneOf,
    PublishDiagnosticsParams, ServerCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind,
    WorkspaceFolder, WorkspaceFoldersServerCapabilities, WorkspaceServerCapabilities,
};
use triomphe::Arc;

pub struct LspServer {
    sender: crossbeam_channel::Sender<Message>,
    db: Db,
    live_docs: LiveDocs,
    diagnostics: Worker<DiagnosticsWorker>,
    ws_worker: Worker<WorkspaceWorker>,
    /// Whether the client's watchers are registered per folder
    watch_folders: bool,
    shutdown_requested: bool,
}

impl LspServer {
    pub fn new(connection: Connection) -> anyhow::Result<(Self, crossbeam_channel::Receiver<Message>)> {
        let (initialize_id, initialize_params) = connection.initialize_start().context("failed to start LSP initialize handshake")?;

        let params: InitializeParams = serde_json::from_value(initialize_params)
            .context("failed to deserialize InitializeParams from client")?;

        // Block on workspace discovery; file contents are read when the
        // database is built, after the handshake is answered.
        let roots = workspace_roots(&params)?;
        let workspace = discover_workspaces(&roots);
        log_info("Workspace discovered");

        let ignored_roots: Vec<String> = workspace.ignored_roots.iter()
            .map(|root| to_url(root).to_string())
            .collect();

        let initialize_result = InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".to_string(), "{".to_string(), ",".to_string()]),
                    resolve_provider: Some(false),
                    ..Default::default()
                }),
                // The session loads every workspace folder the client opened
                // so we listen on folders being added or removed.
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                        supported: Some(true),
                        change_notifications: Some(OneOf::Left(true)),
                    }),
                    file_operations: None,
                }),
                experimental: Some(serde_json::json!({
                    "ignoredRoots": ignored_roots,
                })),
                ..ServerCapabilities::default()
            },
            server_info: Some(lsp_types::ServerInfo {
                name: "solrepr".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        };

        connection.initialize_finish(initialize_id, serde_json::to_value(initialize_result)?)
            .context("failed to finish LSP initialize handshake")?;
        log_info("LSP initialize handshake completed");

        // This is the first moment the spec allows a server-to-client request
        // The client owns the watch; this only tells it what to watch
        watch::register_watchers(&connection.sender, &params, &roots).context("failed to register watched files")?;
        let watch_folders = watch::registers_per_folder(&params);

        let db = Db::new(workspace);
        let diagnostics = Worker::spawn("diagnostics", 8, DiagnosticsWorker::default());
        let ws_worker = Worker::spawn("ws", 8, WorkspaceWorker::new(roots, db.workspace()));
        log_info("DB Initialized");

        Ok((Self {
            sender: connection.sender,
            db,
            diagnostics,
            live_docs: LiveDocs::new(),
            ws_worker,
            watch_folders,
            shutdown_requested: false,
        }, connection.receiver))
    }

    pub fn run(
        mut self,
        receiver: crossbeam_channel::Receiver<Message>,
    ) -> anyhow::Result<()> {
        loop {
            // Biased, with reload first: a finished diff is a root-set change
            // that a pending watcher event has to see. If a `.sol` creation
            // were handled before the diff that adds its root, the path would
            // look unrooted and the event would be answered by a rediscovery
            // that finds nothing new.
            crossbeam_channel::select_biased! {
                recv(self.ws_worker.rx) -> workspace => {
                    match workspace {
                        Ok(diff) => self.handle_reload(diff),
                        Err(_) => {
                            log_info("reload worker stopped; the workspace is fixed for this session");
                            self.ws_worker.stop();
                        }
                    }
                }
                recv(receiver) -> message => {
                    // A closed channel means the client is gone.
                    let Ok(message) = message else { break; };
                    match message {
                        Message::Request(request) => match request.method.as_str() {
                            Shutdown::METHOD => self.shutdown(request.id),
                            _ if self.shutdown_requested => self.reject(request.id),
                            _ =>  self.handle_request(request),
                        }
                        Message::Notification(notification) => match notification.method.as_str() {
                            Exit::METHOD => return self.exit(),
                            _ if self.shutdown_requested => log_info(format!( "ignoring {} after shutdown", notification.method )),
                            _ => self.handle_notification(notification),
                        }
                        Message::Response(_) => {}
                    }
                }
                recv(self.diagnostics.rx) -> result => {
                    match result {
                        Ok(result) => self.handle_diagnostic_result(result),
                        Err(_) => {
                            log_info("diagnostics worker stopped; diagnostics are disabled");
                            self.diagnostics.stop();
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn handle_request(&self, request: Request) {
        let id = request.id.clone();
        let result = match request.method.as_str() {
            HoverRequest::METHOD => hover(&self.db, request),
            GotoDefinition::METHOD => definition(&self.db, request),
            CompletionRequest::METHOD => completion(&self.db, request),
            ViewHirRequest::METHOD => view_hir(&self.db, request),
            ViewAstRequest::METHOD => view_ast(&self.db, request),
            _ => Err(anyhow::anyhow!("unsupported request: {}", request.method)),
        };
        self.dispatch(id, result);
    }

    fn handle_notification(&mut self, notification: Notification) {
        match notification.method.as_str() {
            DidOpenTextDocument::METHOD => self.did_open(&notification),
            DidChangeTextDocument::METHOD => self.did_change(&notification),
            DidCloseTextDocument::METHOD => self.did_close(&notification),
            DidChangeWatchedFiles::METHOD => self.did_change_watched_files(&notification),
            DidChangeWorkspaceFolders::METHOD => self.did_change_workspace_folders(&notification),
            _ => log_info(format!( "Ignoring unsupported notification: {}", notification.method )),
        }
    }


    fn did_open(&mut self, notification: &Notification) {
        if let Some(params) = self.notification_params::<DidOpenTextDocumentParams>(notification)
            && let Some(path) = document_path(&params.text_document.uri)
            && let Some(file) = self.db.open_file(path, params.text_document.text) 
        {
            // An eviction only leaves the diagnosed set — the evicted
            // document stays open and keeps its last published diagnostics.
            self.live_docs.touch(file, params.text_document.version);
            self.run_diagnostics();
        }
    }

    fn did_change(&mut self, notification: &Notification) {
        if let Some(params) = self.notification_params::<DidChangeTextDocumentParams>(notification)
            && let Some(path) = document_path(&params.text_document.uri) 
            && let Some(file) = self.db.file(&path)
            && let Some(current) = self.live_docs.version(&file)
            && params.text_document.version > current
        {
            self.live_docs.touch(file, params.text_document.version);
            if self.db.apply_changes(path, params.content_changes).is_some() {
                self.run_diagnostics();
            }
        }
    }

    fn did_close(&mut self, notification: &Notification) {
        if let Some(params) = self.notification_params::<DidCloseTextDocumentParams>(notification)
            && let Some(path) = document_path(&params.text_document.uri)
            && let Some(file) = self.db.file(&path)
            && self.live_docs.is_open(&file)
        {
            // Clear the diagnostics for a document we no longer own,
            // otherwise the client keeps showing the last set. Publish
            // before forgetting it so the clear carries its version.
            let version = self.live_docs.version(&file);
            self.publish_diagnostics(file, version, Arc::from(Vec::new()));
            self.live_docs.close(&file);
            self.db.close_file(file);
        }
    }

    /// A watcher event is a trigger, not a decision: a manifest may have added
    /// or removed roots (rediscover), a `.sol` file may have appeared or
    /// vanished inside a known root (one path in, one path out).
    ///
    /// The event's type is not consulted. `reload_path` re-reads the path and
    /// decides from what is on disk, so create, change and delete are one
    /// operation — and an open document is never re-read, because the client
    /// owns its text.
    fn did_change_watched_files(&mut self, notification: &Notification) {
        let Some(params) = self.notification_params::<DidChangeWatchedFilesParams>(notification) else {
            return;
        };

        let mut rediscover = false;
        let mut changed = false;
        for path in watch::changed_paths(&params.changes) {
            match watch::classify(&path) {
                Some(Watched::Manifest) => rediscover = true,
                Some(Watched::Source) => match self.db.apply_file_change(&path) {
                    ChangeEvent::Updated(_) | ChangeEvent::Created(_) => changed = true,
                    ChangeEvent::Removed(file) => {
                        self.publish_diagnostics(file, None, Arc::from(Vec::new()));
                        changed = true;
                    }
                    // Under no root we know: a dependency directory may have
                    // appeared.
                    ChangeEvent::Unrooted => rediscover = true,
                    ChangeEvent::Skipped => {}
                },
                None => {}
            }
        }

        if rediscover {
            self.ws_worker.request(ReloadRequest::Rediscover);
        } else if changed {
            self.run_diagnostics();
        }
    }

    /// The client opened or closed a folder. The delta is all the worker needs:
    /// it owns the folder set, so applying it there keeps the set in one place.
    /// The watchers are per folder, so the same delta registers and revokes
    /// them.
    fn did_change_workspace_folders(&mut self, notification: &Notification) {
        let Some(params) = self
            .notification_params::<DidChangeWorkspaceFoldersParams>(notification)
        else {
            return;
        };

        let added = folder_paths(params.event.added);
        let removed = folder_paths(params.event.removed);
        if added.is_empty() && removed.is_empty() {
            return;
        }

        if self.watch_folders {
            for root in &added {
                if let Err(error) = watch::register_folder(&self.sender, root) {
                    log_info(format!("failed to watch {root}: {error}"));
                }
            }
            for root in &removed {
                if let Err(error) = watch::unregister_folder(&self.sender, root) {
                    log_info(format!("failed to stop watching {root}: {error}"));
                }
            }
        }

        log_info(format!(
            "workspace folders changed: {} added, {} removed",
            added.len(),
            removed.len()
        ));
        self.ws_worker.request(ReloadRequest::Folders {
            added: added.into_boxed_slice(),
            removed: removed.into_boxed_slice(),
        });
    }

    fn handle_reload(&mut self, diff: WorkspaceDiff<LoadedFile>) {
        // Apply the workspace diff to the database
        self.db.reload_workspace(diff);

        log_info("workspace reloaded".to_string());
        // Remove deleted files from the live docs
        let deleted: Vec<_> = self.live_docs.iter().filter(|file| self.db.file(file.path(&self.db)).is_none()).collect();
        for file in deleted {
            let version = self.live_docs.version(&file);
            self.publish_diagnostics(file, version, Arc::from(Vec::new()));
            self.live_docs.close(&file);
            self.db.close_file(file);
        }

        // The params must be an object: JSON-RPC reads an array as positional
        // arguments, so a client spreads it into the handler instead of
        // passing it as one value.
        self.send(Message::Notification(Notification::new(
            "solrepr/ignoredRoots".to_string(),
            serde_json::json!({
                "roots": self.db.ignored_paths().iter()
                    .map(|root| to_url(root).to_string())
                    .collect::<Vec<_>>(),
            }),
        )));

        self.run_diagnostics();
    }

    /// A notification's params, or `None` when they do not fit. A notification
    /// has no reply, so a malformed one is only logged.
    fn notification_params<T: serde::de::DeserializeOwned>(
        &self,
        notification: &Notification,
    ) -> Option<T> {
        match serde_json::from_value(notification.params.clone()) {
            Ok(params) => Some(params),
            Err(error) => {
                log_info(format!("ignoring malformed {}: {error}", notification.method));
                None
            }
        }
    }

    /// Send a capability's response. A capability failure answers `null`
    /// rather than an error — the server is still in beta — but a request
    /// must get exactly one reply, otherwise the client waits.
    fn dispatch(&self, id: RequestId, result: anyhow::Result<Response>) {
        let response = result.unwrap_or_else(|error| {
            log_info(format!("request failed: {error:#}"));
            Response::new_ok(id, serde_json::Value::Null)
        });
        self.send(Message::Response(response));
    }

    fn run_diagnostics(&self) {
        let files = self
            .live_docs
            .iter()
            .filter_map(|file| {
                Some(DiagnosedFile {
                    file,
                    version: self.live_docs.version(&file)?,
                    epoch: self.live_docs.epoch(&file)?,
                })
            })
            .collect::<Vec<_>>();
        if files.is_empty() {
            return;
        }
        self.diagnostics.request(DiagnosticRequest {
            snapshot: self.db.snapshot(),
            files,
        });
    }

    fn handle_diagnostic_result(&mut self, result: DiagnosticResult) {
        // Closed, evicted, or edited again in the gap after the worker's own
        // coalesce check: the newer edit already queued its own pass.
        if !self.live_docs.is_live(&result.file)
            || self.live_docs.version(&result.file) != Some(result.version)
        {
            return;
        }

        self.publish_diagnostics(result.file, Some(result.version), result.diagnostics);
    }

    fn publish_diagnostics(
        &self,
        file: File,
        version: Option<i32>,
        diagnostics: Arc<[LspDiagnostic]>,
    ) {
        self.send(Message::Notification(Notification::new(
            PublishDiagnostics::METHOD.to_string(),
            PublishDiagnosticsParams {
                uri: to_url(file.path(&self.db)),
                diagnostics: diagnostics.to_vec(),
                version,
            },
        )));
    }

    /// A failed send means the client is gone; the receiver half of the loop
    /// ends on its own.
    fn send(&self, message: Message) {
        if let Err(error) = self.sender.send(message) {
            log_info(format!("failed to send message: {error}"));
        }
    }

    /// `exit` ends the process. An exit that was not preceded by `shutdown` is a
    /// protocol failure, which the non-zero status reports to the client.
    fn exit(&self) -> anyhow::Result<()> {
        if self.shutdown_requested {
            Ok(())
        } else {
            Err(anyhow::anyhow!("client sent `exit` without `shutdown`"))
        }
    }

    /// `shutdown` is the client's warning that `exit` comes next.
    fn shutdown(&mut self, id: RequestId) {
        self.shutdown_requested = true;
        self.send(Message::Response(Response::new_ok(id,serde_json::Value::Null)));
    }

    /// After `shutdown` the only message left is `exit`; a late request is a
    /// protocol violation but still gets its one reply so the client does not
    /// wait.
    fn reject(&self, id: RequestId) {
        self.send(Message::Response(Response::new_err(
            id,
            ErrorCode::InvalidRequest as i32,
            "server is shutting down".into(),
        )));
    }
}

/// The `file:` folders among `folders`, in order. A folder the server cannot
/// read is skipped — it has no files to discover.
fn folder_paths(folders: Vec<WorkspaceFolder>) -> Vec<camino::Utf8PathBuf> {
    folders
        .iter()
        .filter_map(|folder| to_utf8path(&folder.uri).ok())
        .collect()
}
