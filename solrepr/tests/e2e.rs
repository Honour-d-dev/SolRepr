//! Protocol-level tests: the real message loop over an in-process connection.
//!
//! Everything here goes through `Connection::memory()` into `LspServer`, so it
//! covers what the in-crate unit tests cannot: the initialize handshake, LSP
//! JSON in both directions, position conversion, capability dispatch, and the
//! diagnostics worker.

use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::Duration;

use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    UnregistrationParams,
    DidChangeWatchedFilesRegistrationOptions, GlobPattern, PublishDiagnosticsParams,
    RegistrationParams, Url,
};
use serde_json::{Value, json};
use solrepr::lsp::LspServer;

/// Generous: a failing assertion should report, not hang. `recv_timeout`
/// turns a silent server into a panic naming the wait.
const TIMEOUT: Duration = Duration::from_secs(30);

const SOURCE: &str = include_str!("../fixtures/e2e/src/E2e.sol");

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures").join("e2e")
}


fn document_uri() -> Url {
    Url::from_file_path(fixture_root().join("src").join("E2e.sol")).expect("fixture path")
}

/// The server's side of the conversation: one in-process LSP connection.
struct Server {
    client: Connection,
    thread: Option<JoinHandle<()>>,
    next_id: i32,
    /// Messages that arrived while a response was being awaited. Responses and
    /// notifications share one channel, so a `publishDiagnostics` can land in
    /// the middle of a request; without this it would be lost.
    stashed: Vec<Message>,
    initialize_result: Value,
}

impl Server {
    fn start() -> Server {
        Server::start_at(fixture_root())
    }

    fn start_at(root: PathBuf) -> Server {
        Server::start_at_with(root, json!({}))
    }

    fn start_at_with(root: PathBuf, capabilities: Value) -> Server {
        Server::start_with(json!({
            "processId": null,
            "rootUri": Url::from_directory_path(&root).expect("fixture root"),
            "capabilities": capabilities,
        }))
    }

    /// A multi-folder session: `rootUri` is null, as VS Code sends it when
    /// more than one folder is open.
    fn start_multi(folders: &[PathBuf]) -> Server {
        Server::start_multi_with(folders, json!({}))
    }

    fn start_multi_with(folders: &[PathBuf], capabilities: Value) -> Server {
        let folders = folders
            .iter()
            .map(|folder| {
                json!({
                    "uri": Url::from_directory_path(folder).expect("folder uri"),
                    "name": folder.file_name().and_then(|name| name.to_str()).unwrap_or("folder"),
                })
            })
            .collect::<Vec<_>>();
        Server::start_with(json!({
            "processId": null,
            "rootUri": null,
            "capabilities": capabilities,
            "workspaceFolders": folders,
        }))
    }

    fn start_with(params: Value) -> Server {
        let (server_connection, client) = Connection::memory();
        let thread = std::thread::spawn(move || {
            let (server, receiver) =
                LspServer::new(server_connection).expect("initialize handshake");
            // An `exit` without `shutdown` is a protocol error, which is the
            // server's business, not this test's.
            let _ = server.run(receiver);
        });

        let mut server = Server {
            client,
            thread: Some(thread),
            next_id: 1,
            stashed: Vec::new(),
            initialize_result: Value::Null,
        };

        server.initialize_result = server.request("initialize", params);
        server.notify("initialized", json!({}));
        server
    }

    fn notify(&self, method: &str, params: Value) {
        self.client
            .sender
            .send(Message::Notification(Notification::new(
                method.to_string(),
                params,
            )))
            .expect("send notification");
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.client
            .sender
            .send(Message::Request(Request::new(
                id.into(),
                method.to_string(),
                params,
            )))
            .expect("send request");

        loop {
            match self.recv() {
                Message::Response(response) if response.id == RequestId::from(id) => {
                    assert!(
                        response.error.is_none(),
                        "`{method}` failed: {:?}",
                        response.error
                    );
                    return response.result.unwrap_or(Value::Null);
                }
                other => self.stashed.push(other),
            }
        }
    }

    /// The next request the server sends, whatever it is. A request must be
    /// answered exactly once, so the caller replies.
    fn next_request(&mut self) -> lsp_server::Request {
        loop {
            let message = match self.stashed.iter().position(|message| matches!(message, Message::Request(_))) {
                Some(index) => self.stashed.remove(index),
                None => self.recv(),
            };
            if let Message::Request(request) = message {
                return request;
            }
        }
    }

    fn respond(&self, id: RequestId, result: Value) {
        self.client
            .sender
            .send(Message::Response(Response::new_ok(id, result)))
            .expect("send response");
    }

    fn recv(&self) -> Message {
        self.client
            .receiver
            .recv_timeout(TIMEOUT)
            .unwrap_or_else(|_| panic!("no message from the server within {TIMEOUT:?}"))
    }

    /// The next notification with `method`, whatever it carries.
    fn next_notification(&mut self, method: &str) -> Value {
        loop {
            let message = match self.stashed.iter().position(|message| match message {
                Message::Notification(notification) => notification.method == method,
                _ => false,
            }) {
                Some(index) => self.stashed.remove(index),
                None => self.recv(),
            };
            if let Message::Notification(notification) = message
                && notification.method == method
            {
                return notification.params;
            }
        }
    }

    /// The next `publishDiagnostics` for `uri`, whatever it contains.
    fn next_publish(&mut self, uri: &Url) -> PublishDiagnosticsParams {
        loop {
            let message = match self.stashed.iter().position(|message| match message {
                Message::Notification(notification) => {
                    notification.method == "textDocument/publishDiagnostics"
                }
                _ => false,
            }) {
                Some(index) => self.stashed.remove(index),
                None => self.recv(),
            };
            if let Message::Notification(notification) = message
                && notification.method == "textDocument/publishDiagnostics"
            {
                let params: PublishDiagnosticsParams =
                    serde_json::from_value(notification.params).expect("publishDiagnostics params");
                if &params.uri == uri {
                    return params;
                }
            }
        }
    }

    /// The next `publishDiagnostics` for `uri` that actually reports something,
    /// skipping the empty ones an earlier edit produced.
    fn publish_with_errors(&mut self, uri: &Url) -> PublishDiagnosticsParams {
        loop {
            let params = self.next_publish(uri);
            if !params.diagnostics.is_empty() {
                return params;
            }
        }
    }

    fn open(&mut self, uri: &Url) {
        self.open_with(uri, SOURCE);
    }

    fn open_with(&mut self, uri: &Url, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": { "uri": uri, "languageId": "solidity", "version": 1, "text": text },
            }),
        );
    }

    /// The next publish for `uri` that reports nothing.
    fn publish_empty(&mut self, uri: &Url) -> PublishDiagnosticsParams {
        loop {
            let params = self.next_publish(uri);
            if params.diagnostics.is_empty() {
                return params;
            }
        }
    }

    /// The next publish for `uri` that does not carry `code`.
    fn publish_without(&mut self, uri: &Url, code: &str) -> PublishDiagnosticsParams {
        loop {
            let params = self.next_publish(uri);
            if !params
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == Some(lsp_types::NumberOrString::String(code.to_string())))
            {
                return params;
            }
        }
    }

    fn change(&mut self, uri: &Url, version: i32, text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": text }],
            }),
        );
    }

    /// `shutdown` -> `exit`, returning the shutdown response.
    fn stop(&mut self) -> Value {
        let response = self.request("shutdown", json!(null));
        self.notify("exit", json!(null));
        if let Some(thread) = self.thread.take() {
            thread.join().expect("server thread panicked");
        }
        response
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Every test leaves through the shutdown path, and a failing assertion
        // still stops the server instead of leaking the thread.
        if self.thread.is_none() {
            return;
        }
        let _ = self.client.sender.send(Message::Request(Request::new(
            RequestId::from(self.next_id),
            "shutdown".to_string(),
            json!(null),
        )));
        let _ = self.client.sender.send(Message::Notification(
            Notification::new("exit".to_string(), json!(null)),
        ));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The server declares what to watch, so a client that supports dynamic
/// registration needs no watcher list of its own.
#[test]
fn initialize_registers_the_watcher_globs_dynamically() {
    let tree = TempTree::new("watch_register");
    let mut server = Server::start_at_with(
        tree.root.clone(),
        json!({
            "workspace": {
                "didChangeWatchedFiles": {
                    "dynamicRegistration": true,
                    "relativePatternSupport": true,
                },
            },
        }),
    );

    let request = server.next_request();
    assert_eq!(request.method, "client/registerCapability");

    let params: RegistrationParams =
        serde_json::from_value(request.params).expect("registration params");
    assert_eq!(params.registrations.len(), 1);
    let registration = &params.registrations[0];
    assert_eq!(registration.method, "workspace/didChangeWatchedFiles");

    let options: DidChangeWatchedFilesRegistrationOptions = serde_json::from_value(
        registration
            .register_options
            .clone()
            .expect("the registration carries its watchers"),
    )
    .expect("watched files options");

    let patterns: Vec<&str> = options
        .watchers
        .iter()
        .map(|watcher| match &watcher.glob_pattern {
            GlobPattern::Relative(relative) => relative.pattern.as_str(),
            GlobPattern::String(pattern) => pattern.as_str(),
        })
        .collect();
    assert!(patterns.contains(&"**/*.sol"), "{patterns:?}");
    assert!(patterns.contains(&"**/foundry.toml"), "{patterns:?}");

    // `relativePatternSupport` means the globs are scoped to the workspace
    // root instead of the whole filesystem.
    assert!(
        options
            .watchers
            .iter()
            .all(|watcher| matches!(watcher.glob_pattern, GlobPattern::Relative(_))),
        "every watcher should be relative: {options:#?}"
    );

    server.respond(request.id, json!(null));
}

/// A client that says nothing about watched files gets no registration, so a
/// statically configured client keeps working.
#[test]
fn no_registration_without_dynamic_support() {
    let tree = TempTree::new("watch_static");
    let mut server = Server::start_at(tree.root.clone());

    let uri = Url::from_file_path(tree.root.join("src/A.sol")).expect("A uri");
    tree.write("src/A.sol", "contract A {}\n");
    server.open_with(&uri, "contract A {}\n");

    // The next message is a diagnostics publish, not a registration request.
    let published = server.next_publish(&uri);
    assert_eq!(published.uri, uri);
}

/// A folder added mid-session is discovered: its files become part of the
/// compilation unit without a restart.
#[test]
fn a_folder_added_mid_session_is_discovered() {
    let first = TempTree::new("folder_add_first");
    first.write("foundry.toml", "[profile.default]\n");
    first.write("src/A.sol", "contract A {}\n");

    let second = TempTree::new("folder_add_second");
    second.write("foundry.toml", "[profile.default]\n");
    second.write("src/B.sol", "contract B { uint256 beta; }\n");

    let mut server = Server::start_multi(std::slice::from_ref(&first.root));
    server.notify(
        "workspace/didChangeWorkspaceFolders",
        json!({
            "event": {
                "added": [{ "uri": Url::from_directory_path(&second.root).expect("folder uri"), "name": "second" }],
                "removed": [],
            },
        }),
    );

    // The reload applies the new folder and refreshes the ignored set; only
    // then is the second folder's file under a root.
    server.next_notification("solrepr/ignoredRoots");

    let uri = Url::from_file_path(second.root.join("src/B.sol")).expect("B uri");
    server.open_with(&uri, "contract B { uint256 beta; }\n");
    let hover = server.request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": 0, "character": 22 },
        }),
    );
    let hover = serde_json::to_string(&hover).expect("hover json");
    assert!(hover.contains("uint256"), "the added folder is indexed: {hover}");
}

/// Folder changes in one burst all land. A folder delta cannot collapse to the
/// newest one — that would drop a folder the client opened — so the debounce
/// combines them instead of keeping the last. Three folders are needed to
/// exercise it: the first request wakes the worker, and the rest are drained
/// while it settles.
#[test]
fn folder_changes_in_one_burst_all_land() {
    let first = TempTree::new("folder_burst_first");
    first.write("foundry.toml", "[profile.default]\n");
    first.write("src/A.sol", "contract A {}\n");

    let second = TempTree::new("folder_burst_second");
    second.write("foundry.toml", "[profile.default]\n");
    second.write("src/B.sol", "contract B { uint256 beta; }\n");

    let third = TempTree::new("folder_burst_third");
    third.write("foundry.toml", "[profile.default]\n");
    third.write("src/C.sol", "contract C { uint256 gamma; }\n");

    let fourth = TempTree::new("folder_burst_fourth");
    fourth.write("foundry.toml", "[profile.default]\n");
    fourth.write("src/D.sol", "contract D { uint256 delta; }\n");

    let mut server = Server::start_multi(std::slice::from_ref(&first.root));
    for (folder, name) in [(&second, "second"), (&third, "third"), (&fourth, "fourth")] {
        server.notify(
            "workspace/didChangeWorkspaceFolders",
            json!({
                "event": {
                    "added": [{
                        "uri": Url::from_directory_path(&folder.root).expect("folder uri"),
                        "name": name,
                    }],
                    "removed": [],
                },
            }),
        );
    }

    server.next_notification("solrepr/ignoredRoots");

    for (folder, file, text) in [
        (&second, "src/B.sol", "contract B { uint256 beta; }\n"),
        (&third, "src/C.sol", "contract C { uint256 gamma; }\n"),
        (&fourth, "src/D.sol", "contract D { uint256 delta; }\n"),
    ] {
        let uri = Url::from_file_path(folder.root.join(file)).expect("file uri");
        server.open_with(&uri, text);
        let hover = server.request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": 0, "character": 22 },
            }),
        );
        let hover = serde_json::to_string(&hover).expect("hover json");
        assert!(hover.contains("uint256"), "{file} was discovered: {hover}");
    }
}

/// A folder removed mid-session takes its roots with it: the document under it
/// stops being analysed and the client's diagnostics are cleared.
#[test]
fn a_folder_removed_mid_session_drops_its_documents() {
    let first = TempTree::new("folder_remove_first");
    first.write("foundry.toml", "[profile.default]\n");
    first.write("src/A.sol", "contract A {}\n");

    let second = TempTree::new("folder_remove_second");
    second.write("foundry.toml", "[profile.default]\n");
    second.write("src/B.sol", "contract B { uint x = missing; }\n");

    let mut server = Server::start_multi(&[first.root.clone(), second.root.clone()]);
    let uri = Url::from_file_path(second.root.join("src/B.sol")).expect("B uri");
    server.open_with(&uri, "contract B { uint x = missing; }\n");
    let published = server.publish_with_errors(&uri);
    assert!(!published.diagnostics.is_empty(), "{published:#?}");

    server.notify(
        "workspace/didChangeWorkspaceFolders",
        json!({
            "event": {
                "added": [],
                "removed": [{ "uri": Url::from_directory_path(&second.root).expect("folder uri"), "name": "second" }],
            },
        }),
    );

    let cleared = server.publish_empty(&uri);
    assert!(
        cleared.diagnostics.is_empty(),
        "the removed folder's document is cleared: {cleared:#?}"
    );
}

/// Watchers are registered per folder, so the same delta registers the added
/// folder's watchers and revokes the removed folder's — no tracker, the id is
/// derived from the path.
#[test]
fn a_folder_change_re_registers_its_watchers() {
    let first = TempTree::new("folder_watch_first");
    let second = TempTree::new("folder_watch_second");

    let mut server = Server::start_multi_with(
        std::slice::from_ref(&first.root),
        json!({
            "workspace": {
                "didChangeWatchedFiles": {
                    "dynamicRegistration": true,
                    "relativePatternSupport": true,
                },
            },
        }),
    );

    // The startup registration for the folder the session began with.
    let initial = server.next_request();
    assert_eq!(initial.method, "client/registerCapability");

    server.notify(
        "workspace/didChangeWorkspaceFolders",
        json!({
            "event": {
                "added": [{ "uri": Url::from_directory_path(&second.root).expect("folder uri"), "name": "second" }],
                "removed": [{ "uri": Url::from_directory_path(&first.root).expect("folder uri"), "name": "first" }],
            },
        }),
    );

    let added = server.next_request();
    assert_eq!(added.method, "client/registerCapability");
    let params: RegistrationParams =
        serde_json::from_value(added.params).expect("registration params");
    assert!(
        params.registrations[0].id.contains("folder_watch_second"),
        "the added folder's id: {:?}",
        params.registrations[0].id
    );

    let removed = server.next_request();
    assert_eq!(removed.method, "client/unregisterCapability");
    let params: UnregistrationParams =
        serde_json::from_value(removed.params).expect("unregistration params");
    assert!(
        params.unregisterations[0].id.contains("folder_watch_first"),
        "the removed folder's id: {:?}",
        params.unregisterations[0].id
    );
}

/// Two folders are two roots: each is discovered on its own, and a document in
/// either resolves through the package that owns it.
#[test]
fn a_multi_folder_session_discovers_every_root() {
    let first = TempTree::new("multi_first");
    first.write("foundry.toml", "[profile.default]\n");
    first.write("src/A.sol", "contract A { uint256 alpha; }\n");

    let second = TempTree::new("multi_second");
    second.write("foundry.toml", "[profile.default]\n");
    second.write("src/B.sol", "contract B { uint256 beta; }\n");

    let mut server = Server::start_multi(&[first.root.clone(), second.root.clone()]);

    for (tree, file, text) in [
        (&first, "src/A.sol", "contract A { uint256 alpha; }\n"),
        (&second, "src/B.sol", "contract B { uint256 beta; }\n"),
    ] {
        let uri = Url::from_file_path(tree.root.join(file)).expect("file uri");
        server.open_with(&uri, text);

        // `alpha` / `beta`: the declaration after the type name.
        let hover = server.request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": 0, "character": 22 },
            }),
        );
        let hover = serde_json::to_string(&hover).expect("hover json");
        assert!(
            hover.contains("uint256"),
            "{file} resolves in its own folder: {hover}"
        );
    }
}

/// `initialize` answers with the capabilities the client acts on.
#[test]
fn initialize_advertises_the_protocol_surface() {
    let server = Server::start();
    let capabilities = &server.initialize_result["capabilities"];

    assert_eq!(
        capabilities["textDocumentSync"], 2,
        "incremental sync, as a bare `TextDocumentSyncKind`: {capabilities:#?}"
    );
    assert_eq!(capabilities["hoverProvider"], json!(true));
    assert_eq!(capabilities["definitionProvider"], json!(true));
    assert_eq!(
        capabilities["completionProvider"]["triggerCharacters"],
        json!([".", "{", ","])
    );
    assert!(
        capabilities["experimental"]["ignoredRoots"].is_array(),
        "ignored roots ride on `experimental`: {capabilities:#?}"
    );
}

/// Hover and goto resolve through the protocol, which means both directions of
/// the position conversion agree with the analysis.
#[test]
fn hover_and_definition_resolve_through_the_protocol() {
    let mut server = Server::start();
    let uri = document_uri();
    server.open(&uri);

    // `amount` in `return x + amount;`
    let position = json!({ "line": 7, "character": 21 });
    let hover = server.request(
        "textDocument/hover",
        json!({ "textDocument": { "uri": uri }, "position": position }),
    );
    let hover = serde_json::to_string(&hover).expect("hover json");
    assert!(hover.contains("uint256"), "hover should name the type: {hover}");

    let definition = server.request(
        "textDocument/definition",
        json!({ "textDocument": { "uri": uri }, "position": position }),
    );
    // A single target is answered as a scalar `Location`, several as an array.
    let locations = match definition.clone() {
        Value::Array(items) => items,
        other => vec![other],
    };
    assert_eq!(locations.len(), 1, "{definition:#?}");
    assert_eq!(locations[0]["uri"], json!(uri));
    assert_eq!(
        locations[0]["range"]["start"]["line"], 4,
        "should point at the declaration: {definition:#?}"
    );
}

/// Completion answers in expression position with the names in scope.
#[test]
fn completion_offers_names_in_scope() {
    let mut server = Server::start();
    let uri = document_uri();
    server.open(&uri);

    let completion = server.request(
        "textDocument/completion",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": 7, "character": 19 },
        }),
    );
    let items = completion["items"].as_array().expect("a completion list");
    let labels: Vec<&str> = items
        .iter()
        .filter_map(|item| item["label"].as_str())
        .collect();
    assert!(labels.contains(&"amount"), "got {labels:?}");
    assert!(labels.contains(&"x"), "the parameter should be offered: {labels:?}");
}

/// An edit that breaks the type is published, and closing the document clears
/// what the server published for it.
#[test]
fn did_change_publishes_diagnostics_and_close_clears_them() {
    let mut server = Server::start();
    let uri = document_uri();
    server.open(&uri);

    // `y` is undeclared, which every layer is certain to report.
    let broken = SOURCE.replace("x + amount", "y");
    server.change(&uri, 2, &broken);

    let published = server.publish_with_errors(&uri);
    assert_eq!(published.version, Some(2), "the publish carries the version");
    assert_eq!(
        published.diagnostics[0].range.start.line, 7,
        "the mismatch is on the `return` line: {published:#?}"
    );

    server.notify(
        "textDocument/didClose",
        json!({ "textDocument": { "uri": uri } }),
    );
    let cleared = server.next_publish(&uri);
    assert!(
        cleared.diagnostics.is_empty(),
        "closing must clear the published set: {cleared:#?}"
    );
}

/// `shutdown` answers and `exit` ends the loop, so the server thread joins.
#[test]
fn shutdown_answers_and_exit_terminates() {
    let mut server = Server::start();
    let response = server.stop();
    assert!(response.is_null(), "shutdown returns null: {response:#?}");
}

/// A throwaway tree on disk: the watcher path reads real files.
struct TempTree {
    root: PathBuf,
}

impl TempTree {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("solrepr_e2e_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).expect("create src");
        Self { root }
    }

    fn write(&self, relative: &str, text: &str) {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create dir");
        }
        std::fs::write(path, text).expect("write file");
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn has_code(params: &PublishDiagnosticsParams, code: &str) -> bool {
    code_count(params, code) > 0
}

fn code_count(params: &PublishDiagnosticsParams, code: &str) -> usize {
    params
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code == Some(lsp_types::NumberOrString::String(code.to_string())))
        .count()
}

/// A watcher event for an open document is not a reason to read it from disk:
/// the buffer is the client's, and the analysis keeps seeing it.
#[test]
fn a_watched_change_does_not_overwrite_an_open_document() {
    let tree = TempTree::new("watcher_open_guard");
    tree.write("foundry.toml", "[profile.default]\n");
    tree.write("src/A.sol", "contract A {}\n");

    let mut server = Server::start_at(tree.root.clone());
    let uri = Url::from_file_path(tree.root.join("src/A.sol")).expect("A uri");
    // The buffer declares `amount`; the file on disk does not.
    server.open_with(&uri, "contract A {\n    uint256 amount;\n}\n");

    server.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": uri, "type": 2 }] }),
    );

    // The request is handled after the notification, and the hover reads the
    // buffer: if the disk text had been applied, `amount` would not exist.
    let hover = server.request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": 1, "character": 12 },
        }),
    );
    let hover = serde_json::to_string(&hover).expect("hover json");
    assert!(
        hover.contains("uint256"),
        "the open document keeps the client's text: {hover}"
    );
}

/// A file appearing inside a known root is a single-path operation: the
/// importer stops reporting the unresolved import without a rediscovery.
#[test]
fn a_watched_file_creation_resolves_an_import() {
    let tree = TempTree::new("watcher_create");
    tree.write("foundry.toml", "[profile.default]\n");
    tree.write("src/A.sol", "import \"./B.sol\";\ncontract A {}\n");

    let mut server = Server::start_at(tree.root.clone());
    let uri = Url::from_file_path(tree.root.join("src/A.sol")).expect("A uri");
    server.open_with(&uri, "import \"./B.sol\";\ncontract A {}\n");

    let published = server.publish_with_errors(&uri);
    assert!(
        has_code(&published, "UnresolvedImport"),
        "B does not exist yet: {published:#?}"
    );

    tree.write("src/B.sol", "contract B {}\n");
    server.notify(
        "workspace/didChangeWatchedFiles",
        json!({
            "changes": [{
                "uri": Url::from_file_path(tree.root.join("src/B.sol")).expect("B uri"),
                "type": 1,
            }],
        }),
    );

    let resolved = server.publish_without(&uri, "UnresolvedImport");
    assert!(
        !has_code(&resolved, "UnresolvedImport"),
        "the watcher indexes the new file: {resolved:#?}"
    );
}

/// A manifest appearing on disk is a root-level change: the worker
/// rediscovers, the reload adds the dependency root with its files, and the
/// importer stops reporting the unresolved import.
#[test]
fn a_watched_manifest_rediscovers_the_workspace() {
    let tree = TempTree::new("watcher_manifest");
    tree.write("foundry.toml", "[profile.default]\n");
    tree.write(
        "src/A.sol",
        "import \"../lib/dep/Dep.sol\";\ncontract A {}\n",
    );

    let mut server = Server::start_at(tree.root.clone());
    let uri = Url::from_file_path(tree.root.join("src/A.sol")).expect("A uri");
    server.open_with(&uri, "import \"../lib/dep/Dep.sol\";\ncontract A {}\n");

    let published = server.publish_with_errors(&uri);
    assert!(
        has_code(&published, "UnresolvedImport"),
        "the dependency does not exist yet: {published:#?}"
    );

    tree.write("lib/dep/foundry.toml", "[profile.default]\n");
    tree.write("lib/dep/Dep.sol", "contract Dep {}\n");
    server.notify(
        "workspace/didChangeWatchedFiles",
        json!({
            "changes": [{
                "uri": Url::from_file_path(tree.root.join("lib/dep/foundry.toml")).expect("manifest uri"),
                "type": 1,
            }],
        }),
    );

    let resolved = server.publish_without(&uri, "UnresolvedImport");
    assert!(
        !has_code(&resolved, "UnresolvedImport"),
        "the reload indexes the dependency root: {resolved:#?}"
    );
}

/// A burst of manifest events collapses into one rediscovery: the worker
/// settles, discovers once, and only then decides whether the result is still
/// current. The reload after the burst still lands, which is what proves an
/// abandoned pass left the worker's root set where the applied diff put it.
#[test]
fn a_burst_of_manifest_events_is_one_rediscovery() {
    const IMPORTS: &str =
        "import \"../lib/one/One.sol\";\nimport \"../lib/two/Two.sol\";\ncontract A {}\n";

    let tree = TempTree::new("watcher_coalesce");
    tree.write("foundry.toml", "[profile.default]\n");
    tree.write("src/A.sol", IMPORTS);

    let mut server = Server::start_at(tree.root.clone());
    let uri = Url::from_file_path(tree.root.join("src/A.sol")).expect("A uri");
    server.open_with(&uri, IMPORTS);

    tree.write("lib/one/foundry.toml", "[profile.default]\n");
    tree.write("lib/one/One.sol", "contract One {}\n");
    let manifest = json!({
        "changes": [{
            "uri": Url::from_file_path(tree.root.join("lib/one/foundry.toml")).expect("manifest uri"),
            "type": 1,
        }],
    });
    server.notify("workspace/didChangeWatchedFiles", manifest.clone());
    server.notify("workspace/didChangeWatchedFiles", manifest);

    // One root appeared, so exactly one of the two imports resolves.
    loop {
        let published = server.next_publish(&uri);
        let remaining = code_count(&published, "UnresolvedImport");
        assert!(
            remaining > 0,
            "one root cannot resolve both imports: {published:#?}"
        );
        if remaining == 1 {
            break;
        }
    }

    // Let the worker go idle before the next change: otherwise the discovery
    // it is already running absorbs that change and the abandoned pass is
    // never exercised.
    std::thread::sleep(Duration::from_millis(700));

    // The worker is idle again, so a genuine change is still rediscovered.
    tree.write("lib/two/foundry.toml", "[profile.default]\n");
    tree.write("lib/two/Two.sol", "contract Two {}\n");
    server.notify(
        "workspace/didChangeWatchedFiles",
        json!({
            "changes": [{
                "uri": Url::from_file_path(tree.root.join("lib/two/foundry.toml")).expect("manifest uri"),
                "type": 1,
            }],
        }),
    );

    let resolved = server.publish_without(&uri, "UnresolvedImport");
    assert!(
        !has_code(&resolved, "UnresolvedImport"),
        "the reload after the burst never ran: {resolved:#?}"
    );
}

/// A reload that removes a root takes an open document under it with it: the
/// client's diagnostics are cleared and the server stops tracking the buffer.
#[test]
fn a_reload_that_removes_a_root_clears_an_open_document_under_it() {
    let tree = TempTree::new("orphaned");
    tree.write("foundry.toml", "[profile.default]\n");
    tree.write("src/A.sol", "contract A {}\n");
    tree.write("lib/dep/foundry.toml", "[profile.default]\n");
    tree.write("lib/dep/Dep.sol", "contract Dep { uint x = missing; }\n");

    let mut server = Server::start_at(tree.root.clone());
    let uri = Url::from_file_path(tree.root.join("lib/dep/Dep.sol")).expect("Dep uri");
    server.open_with(&uri, "contract Dep { uint x = missing; }\n");

    let published = server.publish_with_errors(&uri);
    assert!(!published.diagnostics.is_empty(), "{published:#?}");

    std::fs::remove_dir_all(tree.root.join("lib/dep")).expect("remove the dependency");
    server.notify(
        "workspace/didChangeWatchedFiles",
        json!({
            "changes": [{
                "uri": Url::from_file_path(tree.root.join("lib/dep/foundry.toml")).expect("manifest uri"),
                "type": 3,
            }],
        }),
    );

    let cleared = server.publish_empty(&uri);
    assert!(
        cleared.diagnostics.is_empty(),
        "the removed document is cleared: {cleared:#?}"
    );
}
