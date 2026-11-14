use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use lsp_types::{Diagnostic as LspDiagnostic, DiagnosticSeverity, NumberOrString};
use ropey::Rope;
use rustc_hash::FxHashMap;
use tree_sitter::Node;
use triomphe::Arc;

use crate::ast::NodeRange;
use crate::ast::kinds::NodeKind;
use crate::hir::diagnostics::{
    Diagnostic, DiagnosticCode, Severity, collect_diagnostics, normalize_diagnostics,
};
use crate::salsa::{DbSnapshot, File, HirDb};
use crate::utilities::{byte_to_position, to_url};
use crate::worker::{Job, coalesce, debounce};

/// A pass answers an edit, so it is worth waiting out a burst of keystrokes
/// before converting anything.
const DEBOUNCE: Duration = Duration::from_millis(50);

/// One conversion pass: the snapshot to read, and which documents to convert
/// with the version and open epoch each was read at.
pub(crate) struct DiagnosticRequest {
    pub(crate) snapshot: DbSnapshot,
    pub(crate) files: Vec<DiagnosedFile>,
}

/// A live document as the pass saw it. The version stamps the publish, and the
/// epoch distinguishes a reopened document from the one that was closed.
#[derive(Clone, Copy)]
pub(crate) struct DiagnosedFile {
    pub(crate) file: File,
    pub(crate) version: i32,
    pub(crate) epoch: u64,
}

pub(crate) struct DiagnosticResult {
    pub(crate) file: File,
    pub(crate) version: i32,
    pub(crate) diagnostics: Arc<[LspDiagnostic]>,
}

/// Converts diagnostics off the main loop, streaming one result per document.
///
/// `published` is what the client was last sent, per document. Without it every
/// pass republishes every open document, and one keystroke 
/// costs a conversion, a copy and a notification per tab.
#[derive(Default)]
pub(crate) struct DiagnosticsWorker {
    published: FxHashMap<File, Published>,
}

#[derive(Clone)]
struct Published {
    epoch: u64,
    version: i32,
    diagnostics: Arc<[LspDiagnostic]>,
}

impl DiagnosticsWorker {
    fn unchanged(&self, file: File, epoch: u64, version: i32, diagnostics: &Arc<[LspDiagnostic]>) -> bool {
        self.published.get(&file).is_some_and(|published| {
            published.epoch == epoch
                && published.version == version
                && same(&published.diagnostics, diagnostics)
        })
    }

    /// Drops entries for documents that left the diagnosed set since the last
    /// pass, so the map cannot outgrow the live set.
    fn retain_live(&mut self, files: &[DiagnosedFile]) {
        if self.published.len() <= files.len() {
            return;
        }
        let live = files.iter().map(|file| file.file).collect::<rustc_hash::FxHashSet<_>>();
        self.published.retain(|file, _| live.contains(file));
    }

    /// One pass: convert what changed, hand each result to the loop, and stop
    /// early if a newer request arrives.
    ///
    /// Committing before sending is deliberate: the entry means "last handed to
    /// the loop", which is what the next pass has to compare against.
    pub(super) fn pass(
        &mut self,
        request: &DiagnosticRequest,
        rx: &Receiver<DiagnosticRequest>,
        tx: &Sender<DiagnosticResult>,
    ) -> Option<DiagnosticRequest> {
        self.retain_live(&request.files);

        for diagnosed in &request.files {
            let diagnostics = match request.snapshot.lsp_diagnostics(diagnosed.file) {
                Ok(diagnostics) => diagnostics,
                // Cancelled: a write bumped the revision, and the request that
                // write queued resumes the pass.
                Err(_) => return None,
            };
            if self.unchanged(diagnosed.file, diagnosed.epoch, diagnosed.version, &diagnostics) {
                continue;
            }
            self.published.insert(
                diagnosed.file,
                Published {
                    epoch: diagnosed.epoch,
                    version: diagnosed.version,
                    diagnostics: diagnostics.clone(),
                },
            );
            if tx.send(DiagnosticResult {
                    file: diagnosed.file,
                    version: diagnosed.version,
                    diagnostics,
                })
                .is_err()
            {
                return None;
            }
            // Every request carries the whole live set, so abandoning this one
            // loses nothing: the newer request covers the rest.
            if let Some(newer) = coalesce(rx, |_, newer| newer) {
                return Some(newer);
            }
        }
        None
    }
}

impl Job for DiagnosticsWorker {
    type Req = DiagnosticRequest;
    type Res = DiagnosticResult;

    fn run(mut self, rx: Receiver<Self::Req>, tx: Sender<Self::Res>) {
        let mut pending = None;

        while let Ok(request) = match pending.take() {
            Some(request) => Ok(request),
            None => rx.recv(),
        } {
            let request = debounce(&rx, request, DEBOUNCE, |_, newer| newer);

            if let Some(newer) = self.pass(&request, &rx, &tx) {
                pending = Some(newer);
            }
        }
    }
}


fn same(a: &Arc<[LspDiagnostic]>, b: &Arc<[LspDiagnostic]>) -> bool {
    Arc::ptr_eq(a, b) || a == b
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




