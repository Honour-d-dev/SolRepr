use crate::capabilities::completion::completion;
use crate::capabilities::definition::definition;
use crate::capabilities::diagnostics::{DiagnosticResult, DiagnosticsWorker};
use crate::capabilities::hover::hover;
use crate::capabilities::view_ast::{ViewAstRequest, view_ast};
use crate::capabilities::view_hir::{ViewHirRequest, view_hir};
use crate::live_docs::LiveDocs;
use crate::workspace::load_workspace;
use crate::salsa::{File, Db};
use crate::utilities::{document_path, log_info, to_url};

use anyhow::Context;
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, Exit,
    Notification as LspNotification, PublishDiagnostics,
};
use lsp_types::request::{
    Completion as CompletionRequest, GotoDefinition, HoverRequest, Request as LspRequest, Shutdown,
};
use lsp_types::{
    CompletionOptions, Diagnostic as LspDiagnostic, DidChangeTextDocumentParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, HoverProviderCapability, InitializeParams,
    InitializeResult, OneOf, PublishDiagnosticsParams, ServerCapabilities,
    TextDocumentSyncCapability, TextDocumentSyncKind,
};

pub(crate) struct SolidityLspServer {
    sender: crossbeam_channel::Sender<Message>,
    db: Db,
    live_docs: LiveDocs,
    diagnostics: DiagnosticsWorker,
    diagnostics_generation: u64,
    shutdown_requested: bool,
}

impl SolidityLspServer {
    pub(crate) fn new(connection: &Connection) -> anyhow::Result<Self> {
        let (initialize_id, initialize_params) = connection.initialize_start().context("failed to start LSP initialize handshake")?;

        let params: InitializeParams = serde_json::from_value(initialize_params)
            .context("failed to deserialize InitializeParams from client")?;

        // Block on workspace loading
        let workspace = load_workspace(&params)?;
        log_info("Workspace fully loaded");

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
                    trigger_characters: Some(vec![
                        ".".to_string(),
                        "{".to_string(),
                        ",".to_string(),
                    ]),
                    resolve_provider: Some(false),
                    ..Default::default()
                }),
                experimental: Some(serde_json::json!({
                    "ignoredRoots": ignored_roots,
                })),
                ..ServerCapabilities::default()
            },
            server_info: Some(lsp_types::ServerInfo {
                name: "solidity-lsp".to_string(),
                version: None,
            }),
        };

        connection.initialize_finish(initialize_id, serde_json::to_value(initialize_result)?)
            .context("failed to finish LSP initialize handshake")?;
        log_info("LSP initialize handshake completed");

        let db = Db::new(workspace);
        let diagnostics = DiagnosticsWorker::spawn();
        log_info("DB Initialized");

        Ok(Self {
            sender: connection.sender.clone(),
            db,
            diagnostics,
            live_docs: LiveDocs::new(),
            diagnostics_generation: 0,
            shutdown_requested: false,
        })
    }

    pub(crate) fn run(
        mut self,
        receiver: crossbeam_channel::Receiver<Message>,
    ) -> anyhow::Result<()> {
        loop {
            crossbeam_channel::select! {
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
            self.publish_diagnostics(file, Vec::new());
            self.live_docs.close(&file);
        }
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

    fn run_diagnostics(&mut self) {
        if self.live_docs.is_empty() {
            return;
        }

        self.diagnostics_generation += 1;
        let generation = self.diagnostics_generation;
        let documents = self.live_docs.iter().collect();
        self.diagnostics
            .request(self.db.snapshot(), generation, documents);
    }

    fn handle_diagnostic_result(&mut self, result: DiagnosticResult) {
        if result.generation != self.diagnostics_generation
            || !self.live_docs.is_live(&result.file)
        {
            return;
        }

        self.publish_diagnostics(result.file, result.diagnostics);
    }

    fn publish_diagnostics(&self, file: File, diagnostics: Vec<LspDiagnostic>) {
        self.send(Message::Notification(Notification::new(
            PublishDiagnostics::METHOD.to_string(),
            PublishDiagnosticsParams {
                uri: to_url(file.path(&self.db)),
                diagnostics,
                version: self.live_docs.version(&file),
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
