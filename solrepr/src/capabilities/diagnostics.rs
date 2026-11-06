use std::thread;

use crossbeam_channel::{Receiver, Sender, unbounded};
use lsp_types::{Diagnostic as LspDiagnostic, DiagnosticSeverity, NumberOrString};
use ropey::Rope;
use tree_sitter::Node;
use triomphe::Arc;

use crate::ast::NodeRange;
use crate::ast::kinds::NodeKind;
use crate::hir::diagnostics::{
    Diagnostic, DiagnosticCode, Severity, collect_diagnostics, normalize_diagnostics,
};
use crate::salsa::{DbSnapshot, File, HirDb};
use crate::utilities::{byte_to_position, to_url};

pub(crate) struct DiagnosticsWorker {
    pub(crate) tx: Sender<DiagnosticRequest>,
    pub(crate) rx: Receiver<DiagnosticResult>,
}

pub(crate) struct DiagnosticResult {
    pub file: File,
    pub generation: u64,
    pub diagnostics: Arc<[LspDiagnostic]>,
}

pub(crate) struct DiagnosticRequest {
    snapshot: DbSnapshot,
    generation: u64,
    files: Vec<File>,
}

impl DiagnosticsWorker {
    pub fn spawn() -> Self {
        let (tx_request, rx_request) = unbounded();
        let (tx_result, rx_result) = unbounded();
        thread::Builder::new()
            .name("solrepr-diagnostics".to_string())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || run_worker(rx_request, tx_result))
            .expect("failed to spawn diagnostics worker");
        Self {
            tx: tx_request,
            rx: rx_result,
        }
    }

    pub fn request(&self, snapshot: DbSnapshot, generation: u64, files: Vec<File>) {
        let _ = self.tx.send(DiagnosticRequest {
            snapshot,
            generation,
            files,
        });
    }

    /// Stop listening for results: the worker thread is gone, so selecting on
    /// its channel would otherwise return immediately on every loop turn.
    pub fn stop(&mut self) {
        self.rx = crossbeam_channel::never();
    }
}

fn run_worker(receiver: Receiver<DiagnosticRequest>, output: Sender<DiagnosticResult>) {
    while let Ok(mut request) = receiver.recv() {
        // Every request reads the current revision, so a queued one is stale
        // the moment a newer one exists: only the newest is worth converting.
        while let Ok(newer) = receiver.try_recv() {
            request = newer;
        }
        for file in request.files.iter().copied() {
            let diagnostics = match request.snapshot.lsp_diagnostics(file) {
                Ok(diagnostics) => diagnostics,
                Err(_) => break,
            };
            if output.send(DiagnosticResult {
                    file,
                    generation: request.generation,
                    diagnostics,
                }).is_err()
            {
                return;
            }
        }
    }
}

pub(crate) const MAX_PARSE_DIAGNOSTICS: usize = 100;


#[salsa::tracked]
pub fn lsp_diagnostics(db: &dyn HirDb, file: File) -> Arc<[LspDiagnostic]> {
    let rope = file.text(db);
    let diagnostics = diagnostics(db, file);
    let mut converted = Vec::with_capacity(diagnostics.len());
    for diagnostic in diagnostics.iter() {
        
        db.unwind_if_revision_cancelled();
        converted.push(to_lsp_diagnostic(db, &rope, diagnostic));
    }
    Arc::from(converted)
}


/// Every diagnostic for a file: parse errors plus semantic errors.
pub(crate) fn diagnostics(db: &dyn HirDb, file: File) -> Vec<Diagnostic> {
    let mut diagnostics = parse_errors(db, file);
    diagnostics.extend(collect_diagnostics(db, file));
    normalize_diagnostics(&mut diagnostics);
    diagnostics
}


/// Parse errors for one file.
fn parse_errors(db: &dyn HirDb, file: File) -> Vec<Diagnostic> {
    let ast = db.ast(file);
    let mut diagnostics = Vec::new();
    let root = ast.tree().root_node();
    let mut cursor = root.walk();
    for declaration in root.children(&mut cursor) {
        // @NOTE safe: only called inside a tracked fn
        db.unwind_if_revision_cancelled();
        if diagnostics.len() >= MAX_PARSE_DIAGNOSTICS {
            break;
        }
        collect_parse_errors(declaration, ast.source(), &mut diagnostics);
    }
    diagnostics
}


pub(crate) fn collect_parse_errors(node: Node<'_>, source: &str, diagnostics: &mut Vec<Diagnostic>) {
    if node.kind_id() == NodeKind::INCOMPLETE_MEMBER_EXPRESSION {
        diagnostics.push(Diagnostic {
            code: DiagnosticCode::SyntaxError,
            severity: Severity::Error,
            range: NodeRange::from(&node),
            message: "expected a member name after `.`".to_string(),
            related: Vec::new(),
        });
    } else if node.is_error() {
        let text = source.get(node.byte_range()).unwrap_or_default().trim();
        let message = if text.is_empty() {
            "syntax error".to_string()
        } else {
            format!("unexpected syntax `{text}`")
        };
        diagnostics.push(Diagnostic {
            code: DiagnosticCode::SyntaxError,
            severity: Severity::Error,
            range: NodeRange::from(&node),
            message,
            related: Vec::new(),
        });
    } else if node.is_missing() {
        diagnostics.push(Diagnostic {
            code: DiagnosticCode::SyntaxError,
            severity: Severity::Error,
            range: NodeRange::from(&node),
            message: format!("syntax error: missing `{}`", node.kind()),
            related: Vec::new(),
        });
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if diagnostics.len() >= MAX_PARSE_DIAGNOSTICS {
            break;
        }
        collect_parse_errors(child, source, diagnostics);
    }
}


fn to_lsp_diagnostic(db: &dyn HirDb, rope: &Rope, diagnostic: &Diagnostic) -> LspDiagnostic {
    let related_information = if diagnostic.related.is_empty() {
        None
    } else {
        Some(
            diagnostic.related.iter().map(|r| {
                    let uri = to_url(r.file.path(db).as_path());
                    let related_rope = r.file.text(db);
                    lsp_types::DiagnosticRelatedInformation {
                        location: lsp_types::Location {
                            uri,
                            range: lsp_types::Range {
                                start: byte_to_position(&related_rope, r.range.start as usize),
                                end: byte_to_position(&related_rope, r.range.end as usize),
                            },
                        },
                        message: r.message.clone(),
                    }
                })
                .collect(),
        )
    };
    LspDiagnostic {
        range: lsp_types::Range {
            start: byte_to_position(rope, diagnostic.range.start as usize),
            end: byte_to_position(rope, diagnostic.range.end as usize),
        },
        severity: Some(match diagnostic.severity {
            Severity::Error => DiagnosticSeverity::ERROR,
            Severity::Warning => DiagnosticSeverity::WARNING,
            Severity::Information => DiagnosticSeverity::INFORMATION,
            Severity::Hint => DiagnosticSeverity::HINT,
        }),
        code: Some(NumberOrString::String(format!("{:?}", diagnostic.code))),
        source: Some("solrepr".to_string()),
        message: diagnostic.message.clone(),
        related_information,
        tags: None,
        code_description: None,
        data: None,
    }
}




