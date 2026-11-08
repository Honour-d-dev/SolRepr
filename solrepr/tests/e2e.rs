//! Protocol-level tests: the real message loop over an in-process connection.
//!
//! Everything here goes through `Connection::memory()` into `LspServer`, so it
//! covers what the in-crate unit tests cannot: the initialize handshake, LSP
//! JSON in both directions, position conversion, capability dispatch, and the
//! diagnostics worker.

use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::Duration;

use lsp_server::{Connection, Message, Notification, Request, RequestId};
use lsp_types::{PublishDiagnosticsParams, Url};
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

        let params = json!({
            "processId": null,
            "rootUri": Url::from_directory_path(fixture_root()).expect("fixture root"),
            "capabilities": {},
        });
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

    fn recv(&self) -> Message {
        self.client
            .receiver
            .recv_timeout(TIMEOUT)
            .unwrap_or_else(|_| panic!("no message from the server within {TIMEOUT:?}"))
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
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": { "uri": uri, "languageId": "solidity", "version": 1, "text": SOURCE },
            }),
        );
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
