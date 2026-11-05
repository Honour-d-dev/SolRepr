use std::thread;

use crossbeam_channel::{Receiver, Sender, unbounded};
use lsp_types::{Diagnostic as LspDiagnostic, DiagnosticSeverity, NumberOrString};
use ropey::Rope;
use tree_sitter::Node;
use triomphe::Arc;

use crate::ast::{NodeRange, kinds::NodeKind};
use crate::hir::access::AccessDenial;
use crate::hir::resolution::Callable;
use crate::hir::semantic_errors::{DiagnosticCode, SemanticDiagnostic};
use crate::hir::signatures::OverrideMismatch;
use crate::hir::types::Mutability;
use crate::ir::def_map::DefId;
use crate::salsa::hir_db::def_name;
use crate::salsa::root_db::def_range;
use crate::salsa::{File, HirDb, DbSnapshot};
use crate::utilities::{byte_to_position, to_url};

pub(crate) struct DiagnosticsWorker {
    pub(crate) tx: Sender<DiagnosticRequest>,
    pub(crate) rx: Receiver<DiagnosticResult>,
}

pub(crate) struct DiagnosticResult {
    pub file: File,
    pub generation: u64,
    pub diagnostics: Vec<LspDiagnostic>,
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
            .name("solidity-diagnostics".to_string())
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
    for request in receiver {
        for file in request.files {
            let diagnostics = match request.snapshot.diagnostics(file) {
                Ok(diagnostics) => diagnostics,
                Err(_) => break,
            };
            let rope = match request.snapshot.file_text(file) {
                Ok(rope) => rope,
                Err(_) => break,
            };
            let diagnostics = diagnostics
                .iter()
                .map(|diagnostic| to_lsp_diagnostic(&request.snapshot, &rope, diagnostic))
                .collect();
            if output
                .send(DiagnosticResult {
                    file,
                    generation: request.generation,
                    diagnostics,
                })
                .is_err()
            {
                return;
            }
        }
    }
}

fn to_lsp_diagnostic(
    snapshot: &DbSnapshot,
    rope: &Rope,
    diagnostic: &Diagnostic,
) -> LspDiagnostic {
    let related_information = if diagnostic.related.is_empty() {
        None
    } else {
        Some(
            diagnostic
                .related
                .iter()
                .filter_map(|r| {
                    let path = snapshot.file_path(r.file).ok()?;
                    let uri = to_url(path.as_path());
                    // For cross-file related diagnostics, fetch the related file's rope
                    // to convert byte offsets to positions correctly.
                    let related_rope = snapshot.file_text(r.file).ok()?;
                    Some(lsp_types::DiagnosticRelatedInformation {
                        location: lsp_types::Location {
                            uri,
                            range: lsp_types::Range {
                                start: byte_to_position(&related_rope, r.range.start as usize),
                                end: byte_to_position(&related_rope, r.range.end as usize),
                            },
                        },
                        message: r.message.clone(),
                    })
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



pub(crate) const MAX_PARSE_DIAGNOSTICS: usize = 100;

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Severity {
    Error,
    Warning,
    Information,
    Hint,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelatedDiagnostic {
    pub file: File,
    pub range: NodeRange,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: DiagnosticCode,
    pub severity: Severity,
    pub range: NodeRange,
    pub message: String,
    pub related: Vec<RelatedDiagnostic>,
}

#[salsa::tracked]
pub fn parse_errors(db: &dyn HirDb, file: File) -> Arc<[Diagnostic]> {
    let ast = db.ast(file);
    let mut diagnostics = Vec::new();
    collect_parse_errors(ast.tree().root_node(), ast.source(), &mut diagnostics);
    Arc::from(diagnostics)
}

#[salsa::tracked]
pub fn diagnostics(db: &dyn HirDb, file: File) -> Arc<[Diagnostic]> {
    // A file under a deduplicated library root is outside the compilation
    // unit — no diagnostics, even parse errors.
    if db.is_ignored_path(file.path(db)) {
        return Arc::from(Vec::new());
    }
    let mut diagnostics = parse_errors(db, file).to_vec();
    diagnostics.extend(collect_semantic_errors(db, file));
    normalize_diagnostics(&mut diagnostics);
    Arc::from(diagnostics)
}

/// Coalesces duplicate diagnostics at the same range, retaining distinct
/// messages while merging their related links.
pub(crate) fn normalize_diagnostics(diagnostics: &mut Vec<Diagnostic>) {
    diagnostics.sort_by(|left, right| {
        left.range
            .start
            .cmp(&right.range.start)
            .then(left.range.end.cmp(&right.range.end))
            .then(left.code.cmp(&right.code))
            .then(left.message.cmp(&right.message))
    });

    let mut merged: Vec<Diagnostic> = Vec::with_capacity(diagnostics.len());
    for mut diagnostic in diagnostics.drain(..) {
        match merged.last_mut() {
            Some(previous)
                if previous.range == diagnostic.range
                    && previous.code == diagnostic.code
                    && previous.message == diagnostic.message =>
            {
                for link in diagnostic.related {
                    if !previous.related.contains(&link) {
                        previous.related.push(link);
                    }
                }
            }
            _ => {
                let mut related = Vec::with_capacity(diagnostic.related.len());
                for link in diagnostic.related {
                    if !related.contains(&link) {
                        related.push(link);
                    }
                }
                diagnostic.related = related;
                merged.push(diagnostic);
            }
        }
    }
    *diagnostics = merged;
}

/// One line for a single message, otherwise one bullet per message.
fn bullet_list<S: AsRef<str>>(lines: &[S]) -> String {
    match lines {
        [only] => only.as_ref().to_string(),
        many => many
            .iter()
            .map(|line| format!("• {}", line.as_ref()))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn collect_semantic_errors(db: &dyn HirDb, file: File) -> Vec<Diagnostic> {
    let analysis = db.root_def_map_analysis(db.file_source_root(file));
    let owners = analysis
        .data
        .defs
        .keys()
        .filter(|owner| owner.file_and_id().0 == file)
        .copied()
        .collect::<Vec<_>>();
    let mut diagnostics = analysis
        .diagnostics
        .iter()
        .filter(|error| error.declaration_file() == Some(file))
        .map(|error| semantic_diagnostic(db, DefId::File(file), error))
        .collect::<Vec<_>>();
    for owner in owners {
        let result = db.inference(owner);
        diagnostics.extend(
            result
                .diagnostics
                .iter()
                .map(|error| semantic_diagnostic(db, owner, error)),
        );
        if matches!(owner, DefId::Contract(_) | DefId::Interface(_)) {
            diagnostics.extend(
                db.member_map(owner)
                    .diagnostics
                    .iter()
                    .map(|error| semantic_diagnostic(db, owner, error)),
            );
        }
    }
    diagnostics
}

fn semantic_diagnostic(db: &dyn HirDb, owner: DefId, error: &SemanticDiagnostic) -> Diagnostic {
    let (code, message, related) = match error {
        SemanticDiagnostic::SyntaxError { .. } => (
            DiagnosticCode::SyntaxError,
            "syntax error".to_string(),
            Vec::new(),
        ),
        SemanticDiagnostic::UnresolvedName { name, denials, .. } => (
            DiagnosticCode::UnresolvedName,
            format!("unresolved name `{name}`"),
            access_related(db, denials),
        ),
        SemanticDiagnostic::UnresolvedMember {
            member,
            receiver,
            denials,
            ..
        } => (
            DiagnosticCode::UnresolvedMember,
            format!("unresolved member `{member}` on `{}`", receiver.display(db)),
            access_related(db, denials),
        ),
        SemanticDiagnostic::InheritanceFailure { problems, .. } => (
            DiagnosticCode::InheritanceFailure,
            bullet_list(problems),
            Vec::new(),
        ),
        SemanticDiagnostic::UnresolvedType { name, .. } => (
            DiagnosticCode::UnresolvedType,
            format!("unresolved type `{name}`"),
            Vec::new(),
        ),
        SemanticDiagnostic::AmbiguousMember {
            name, declarations, ..
        } => (
            DiagnosticCode::AmbiguousMember,
            format!(
                "member `{name}` is ambiguous: defined in multiple base contracts without a unique override"
            ),
            conflict_links(db, declarations),
        ),
        SemanticDiagnostic::InvalidArgumentCount {
            expected, actual, ..
        } => (
            DiagnosticCode::InvalidArgumentCount,
            format!("expected {expected} argument(s), found {actual}"),
            Vec::new(),
        ),
        SemanticDiagnostic::NoMatchingOverload { candidates, .. } => {
            let related = candidates
                .iter()
                .filter_map(|c| callable_location(db, c))
                .collect::<Vec<_>>();
            (
                DiagnosticCode::NoMatchingOverload,
                "no matching overload".to_string(),
                related,
            )
        }
        SemanticDiagnostic::AmbiguousOverload { candidates, .. } => {
            let related = candidates
                .iter()
                .filter_map(|c| callable_location(db, c))
                .collect::<Vec<_>>();
            (
                DiagnosticCode::AmbiguousOverload,
                format!("ambiguous overload ({} candidates)", candidates.len()),
                related,
            )
        }
        SemanticDiagnostic::InferenceFailure { message, .. } => (
            DiagnosticCode::InferenceFailure,
            message.clone(),
            Vec::new(),
        ),
        SemanticDiagnostic::StateMutability {
            declared,
            required,
            callee,
            ..
        } => (
            DiagnosticCode::StateMutability,
            format!(
                "Function cannot be declared as {} because {}.",
                declared.as_str(),
                effect_clause(*required)
            ),
            callee
                .iter()
                .filter_map(|callee| {
                    Some(RelatedDiagnostic {
                        file: callee.file(),
                        range: def_range(db, *callee)?,
                        message: format!("`{}` is declared here", def_name(db, *callee)),
                    })
                })
                .collect(),
        ),
        SemanticDiagnostic::TypeMismatch {
            expected, actual, ..
        } => (
            DiagnosticCode::TypeMismatch,
            format!(
                "type mismatch: expected `{}`, found `{}`",
                expected.display(db),
                actual.display(db)
            ),
            Vec::new(),
        ),
        SemanticDiagnostic::UnknownNamedArgument { name, .. } => (
            DiagnosticCode::UnknownNamedArgument,
            format!("named argument `{name}` does not match function declaration"),
            Vec::new(),
        ),
        SemanticDiagnostic::DuplicateNamedArgument { name, .. } => (
            DiagnosticCode::DuplicateNamedArgument,
            format!("duplicate named argument `{name}`"),
            Vec::new(),
        ),
        SemanticDiagnostic::InvalidExplicitConversion { from, to, .. } => (
            DiagnosticCode::InvalidExplicitConversion,
            format!(
                "explicit type conversion not allowed from `{}` to `{}`",
                from.display(db),
                to.display(db)
            ),
            Vec::new(),
        ),
        SemanticDiagnostic::Redeclaration { name, previous, .. } => (
            DiagnosticCode::Redeclaration,
            match name.as_str() {
                "constructor" => "more than one constructor defined".to_string(),
                "fallback" => "only one fallback function is allowed".to_string(),
                "receive" => "only one receive function is allowed".to_string(),
                _ => format!("definition with same name already exists: `{name}`"),
            },
            def_range(db, *previous)
                .map(|range| RelatedDiagnostic {
                    file: previous.file(),
                    range,
                    message: "previous declaration is here".to_string(),
                })
                .into_iter()
                .collect(),
        ),
        SemanticDiagnostic::UnresolvedImport { path, .. } => (
            DiagnosticCode::UnresolvedImport,
            format!("unresolved import: source `{path}` not found"),
            Vec::new(),
        ),
        SemanticDiagnostic::ImportSymbolNotFound { name, path, .. } => (
            DiagnosticCode::ImportSymbolNotFound,
            format!("`{name}` not found in `{path}`"),
            Vec::new(),
        ),
        SemanticDiagnostic::MissingOverride {
            declaration, bases, ..
        } => (
            DiagnosticCode::MissingOverride,
            format!(
                "`{}` overrides an inherited declaration and must be marked `override`",
                def_name(db, *declaration)
            ),
            related_declarations(db, bases, "overridden declaration `{}` is here"),
        ),
        SemanticDiagnostic::UnnecessaryOverride { declaration, .. } => (
            DiagnosticCode::UnnecessaryOverride,
            format!(
                "`{}` is marked `override` but overrides nothing",
                def_name(db, *declaration)
            ),
            Vec::new(),
        ),
        SemanticDiagnostic::NotVirtual {
            declaration, base, ..
        } => (
            DiagnosticCode::NotVirtual,
            format!(
                "cannot override non-virtual `{}`; add `virtual` to the base declaration",
                def_name(db, *declaration)
            ),
            related_declarations(db, &[*base], "non-virtual declaration `{}` is here"),
        ),
        SemanticDiagnostic::IncompatibleOverride {
            declaration,
            base,
            mismatch,
            ..
        } => (
            DiagnosticCode::IncompatibleOverride,
            incompatible_override_message(db, *declaration, *mismatch),
            related_declarations(db, &[*base], "overridden declaration `{}` is here"),
        ),
        SemanticDiagnostic::UnimplementedOverride {
            declaration, base, ..
        } => (
            DiagnosticCode::UnimplementedOverride,
            {
                let noun = match base {
                    DefId::Modifier(_) => "modifier",
                    _ => "function",
                };
                format!(
                    "implemented {noun} `{}` cannot be overridden by an unimplemented {noun}",
                    def_name(db, *base)
                )
            },
            related_declarations(
                db,
                &[*declaration],
                "unimplemented declaration `{}` is here",
            ),
        ),
        SemanticDiagnostic::OverrideListMismatch {
            declaration,
            expected,
            ..
        } => {
            (
                DiagnosticCode::OverrideListMismatch,
                format!(
                    "override list of `{}` does not match its overridden contracts",
                    def_name(db, *declaration)
                ),
                related_declarations(db, expected, "`{}` must be listed in the override list"),
            )
        }
        SemanticDiagnostic::OverrideConflict {
            name, declarations, ..
        } => (
            DiagnosticCode::OverrideConflict,
            format!(
                "derived contract must override `{name}`: two or more bases define it with the same parameter types"
            ),
            conflict_links(db, declarations),
        ),
        SemanticDiagnostic::UnimplementedContract { contract, .. } => (
            DiagnosticCode::UnimplementedContract,
            format!(
                "contract `{}` has unimplemented functions and must be marked `abstract`",
                def_name(db, *contract)
            ),
            Vec::new(),
        ),
        SemanticDiagnostic::MissingVirtual { declaration, .. } => (
            DiagnosticCode::MissingVirtual,
            match declaration {
                DefId::Modifier(_) => format!(
                    "modifier `{}` has no implementation and must be marked `virtual`",
                    def_name(db, *declaration)
                ),
                _ => format!(
                    "function `{}` has no implementation and must be marked `virtual`",
                    def_name(db, *declaration)
                ),
            },
            Vec::new(),
        ),
        SemanticDiagnostic::MissingImplementation { declaration, .. } => (
            DiagnosticCode::MissingImplementation,
            match db.owner(*declaration) {
                Some(DefId::Library(_)) => format!(
                    "library function `{}` must be implemented",
                    def_name(db, *declaration)
                ),
                _ => format!(
                    "free function `{}` must be implemented",
                    def_name(db, *declaration)
                ),
            },
            Vec::new(),
        ),
        SemanticDiagnostic::VirtualInLibrary { declaration, .. } => (
            DiagnosticCode::VirtualInLibrary,
            match declaration {
                DefId::Modifier(_) => "modifiers in a library cannot be `virtual`".to_string(),
                _ => "library functions cannot be `virtual`".to_string(),
            },
            Vec::new(),
        ),
        SemanticDiagnostic::DuplicateMemberSlot {
            declaration,
            previous,
            ..
        } => (
            DiagnosticCode::DuplicateMemberSlot,
            format!(
                "duplicate declaration of `{}` with the same parameter types",
                def_name(db, *declaration)
            ),
            related_declarations(db, &[*previous], "previous declaration is here"),
        ),
        SemanticDiagnostic::InheritedMemberConflict {
            name, declarations, ..
        } => (
            DiagnosticCode::InheritedMemberConflict,
            format!("inherited member `{name}` is defined more than once"),
            conflict_links(db, declarations),
        ),
        SemanticDiagnostic::InvalidDataLocation { ty, location, .. } => {
            let message = format!(
                "data location `{location}` is invalid for type `{}`",
                ty.peel_loc(db).display(db)
            );
            (DiagnosticCode::InvalidDataLocation, message, Vec::new())
        }
        SemanticDiagnostic::DataLocationRequired { ty, .. } => (
            DiagnosticCode::DataLocationRequired,
            format!(
                "data location must be specified for type `{}`",
                ty.peel_loc(db).display(db)
            ),
            Vec::new(),
        ),
    };
    Diagnostic {
        code,
        severity: Severity::Error,
        range: crate::hir::syntax_store::anchor_range(db, owner, error.anchor())
            .unwrap_or(NodeRange { start: 0, end: 0 }),
        message,
        related,
    }
}

/// solc's wording for the effect a body could not afford. `Pure` is never
/// reported: nothing exceeds it.
fn effect_clause(required: Mutability) -> &'static str {
    match required {
        Mutability::View => "this expression (potentially) reads from the environment or state",
        _ => "this expression (potentially) modifies the state",
    }
}

/// One link per conflicting declaration, labelled with the contract that
/// declares it. The name's diagnostic message already says what conflicts.
fn conflict_links(db: &dyn HirDb, defs: &[DefId]) -> Vec<RelatedDiagnostic> {
    defs.iter()
        .filter_map(|def| {
            let owner = db.owner(*def).map_or_else(String::new, |owner| def_name(db, owner));
            Some(RelatedDiagnostic {
                file: def.file_and_id().0,
                range: def_range(db, *def)?,
                message: format!("declared in `{owner}`"),
            })
        })
        .collect()
}

fn related_declarations(
    db: &dyn HirDb,
    defs: &[DefId],
    message: &str,
) -> Vec<RelatedDiagnostic> {
    defs.iter()
        .filter_map(|def| {
            Some(RelatedDiagnostic {
                file: def.file_and_id().0,
                range: def_range(db, *def)?,
                message: message.replace("{}", &def_name(db, *def)),
            })
        })
        .collect()
}

fn incompatible_override_message(
    db: &dyn HirDb,
    declaration: DefId,
    mismatch: OverrideMismatch,
) -> String {
    let name = def_name(db, declaration);
    match mismatch {
        OverrideMismatch::ReturnTypes => {
            format!("return types of `{name}` differ from the overridden declaration")
        }
        OverrideMismatch::ParameterLocations => {
            format!(
                "data locations of parameters of `{name}` differ; they must match when overriding a non-external function"
            )
        }
        OverrideMismatch::ReturnLocations => {
            format!(
                "data locations of return values of `{name}` differ; they must match when overriding a non-external function"
            )
        }
        OverrideMismatch::Visibility => {
            format!("visibility of `{name}` cannot differ from the overridden declaration")
        }
        OverrideMismatch::Mutability => {
            format!("mutability of `{name}` is less strict than the overridden declaration")
        }
        OverrideMismatch::ModifierParameters => {
            format!("parameters of modifier `{name}` differ from the overridden modifier")
        }
        OverrideMismatch::GetterRequiresExternalBase => {
            format!("public state variable `{name}` can only override an external function")
        }
        OverrideMismatch::GetterCannotBeOverridden => {
            format!("public state variable `{name}` cannot be overridden")
        }
    }
}

fn access_related(db: &dyn HirDb, denials: &[AccessDenial]) -> Vec<RelatedDiagnostic> {
    denials
        .iter()
        .filter_map(|denial| {
            Some(RelatedDiagnostic {
                file: denial.def.file(),
                range: def_range(db, denial.def)?,
                message: format!(
                    "`{}` is declared {} and is not visible here",
                    def_name(db, denial.def),
                    denial.vis.as_str()
                ),
            })
        })
        .collect()
}

/// Renders a callable's signature and source location for diagnostic info.
/// Returns a `RelatedDiagnostic` pointing at the callable's definition site.
fn callable_location(db: &dyn HirDb, callable: &Callable) -> Option<RelatedDiagnostic> {
    let def = callable.def()?;
    let (file, Some(ast_id)) = def.file_and_id() else {
        return None;
    };
    let node = db.ast_id_map(file).get_node(&db.root(file), ast_id)?;
    let range = NodeRange::from(&node.node());
    let signature = callable_signature(db, &def)?;
    let path = file.path(db);
    let file_name = path.file_name().unwrap_or_default();
    let line = node.node().start_position().row + 1;
    Some(RelatedDiagnostic {
        file,
        range,
        message: format!("{signature} — {file_name}:{line}"),
    })
}

/// Formats a callable's signature (params + returns) using the same approach as hover.
fn callable_signature(db: &dyn HirDb, def: &DefId) -> Option<String> {
    match def {
        DefId::Function(id) => {
            let data = db.function_data(*id);
            let params = data
                .args
                .iter()
                .map(|id| {
                    let local = &data.parameters[*id];
                    let ty =
                        data.syntax_store.types[*local.ty_ref()].to_string(&data.syntax_store.types);
                    format!("{} {}", ty, local.name())
                })
                .collect::<Vec<_>>()
                .join(", ");
            let ret = data
                .rets
                .iter()
                .map(|id| {
                    let local = &data.parameters[*id];
                    data.syntax_store.types[*local.ty_ref()].to_string(&data.syntax_store.types)
                })
                .collect::<Vec<_>>()
                .join(", ");
            let mut signature = format!("function {}({params})", data.name);
            if !data.vis.as_str().is_empty() {
                signature.push(' ');
                signature.push_str(data.vis.as_str());
            }
            if !data.mutability.as_str().is_empty() {
                signature.push(' ');
                signature.push_str(data.mutability.as_str());
            }
            if !ret.is_empty() {
                signature.push_str(&format!(" returns ({ret})"));
            }
            Some(signature)
        }
        DefId::Event(id) => {
            let data = db.event_data(*id);
            let params = data
                .parameters
                .iter()
                .map(|(_, p)| {
                    let ty = data.syntax_store.types[*p.ty_ref()].to_string(&data.syntax_store.types);
                    format!("{} {}", ty, p.name())
                })
                .collect::<Vec<_>>()
                .join(", ");
            Some(format!("event {}({params})", data.name))
        }
        DefId::Error(id) => {
            let data = db.error_data(*id);
            let params = data
                .parameters
                .iter()
                .map(|(_, p)| {
                    let ty = data.syntax_store.types[*p.ty_ref()].to_string(&data.syntax_store.types);
                    format!("{} {}", ty, p.name())
                })
                .collect::<Vec<_>>()
                .join(", ");
            Some(format!("error {}({params})", data.name))
        }
        DefId::Modifier(id) => {
            let data = db.modifier_data(*id);
            let params = data
                .parameters
                .iter()
                .map(|(_, local)| {
                    let ty =
                        data.syntax_store.types[*local.ty_ref()].to_string(&data.syntax_store.types);
                    format!("{} {}", ty, local.name())
                })
                .collect::<Vec<_>>()
                .join(", ");
            Some(format!("modifier {}({params})", data.name))
        }
        _ => None,
    }
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


