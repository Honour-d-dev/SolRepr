use rustc_hash::FxHashSet;
use triomphe::Arc;

use crate::ast::{ImportId, NodeRange};
use crate::hir::access::AccessDenial;
use crate::hir::exprs::{ExprId, Name};
use crate::hir::resolution::Callable;
use crate::hir::signatures::OverrideMismatch;
use crate::hir::types::{Location, Mutability, Ty, TyNameId, Type};
use crate::ir::def_map::DefId;
use crate::salsa::File;

macro_rules! define_diagnostics {
    (
        $(
            $variant:ident {
                $( $field:ident : $ty:ty ),* $(,)?
            }
        ),* $(,)?
    ) => {
        #[repr(u8)]
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum DiagnosticCode {
            $( $variant ),*
        }

        #[derive(Clone, PartialEq, Eq)]
        pub enum SemanticDiagnostic {
            $(
                $variant {
                    $( $field : $ty ),*
                }
            ),*
        }

        impl SemanticDiagnostic {
            pub fn code(&self) -> DiagnosticCode {
                match self {
                    $(
                        Self::$variant { .. } => DiagnosticCode::$variant,
                    )*
                }
            }

            pub fn range(&self) -> NodeRange {
                match self {
                    $(
                        Self::$variant { range, .. } => *range,
                    )*
                }
            }

            pub fn key(&self) -> DiagnosticKey {
                DiagnosticKey(self.code(), self.range())
            }
        }
    };
}

/// Identifies a diagnostic for deduplication. Codes anchored at a shared
/// contract range rather than a unique syntax node opt out via
/// [`DiagnosticCode::allows_multiple`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DiagnosticKey(DiagnosticCode, NodeRange);

define_diagnostics! {
    SyntaxError {
        range: NodeRange
    },
    UnresolvedName {
        expr: ExprId,
        range: NodeRange,
        name: Name,
        denials: Vec<AccessDenial>,
    },
    UnresolvedMember {
        expr: ExprId,
        range: NodeRange,
        receiver: Ty,
        member: Name,
        denials: Vec<AccessDenial>,
    },
    // A member reference whose slot holds more than one inherited
    // declaration with no effective winner. `declarations` are the
    // conflicting base declarations.
    AmbiguousMember {
        expr: ExprId,
        range: NodeRange,
        name: Name,
        declarations: Vec<DefId>,
    },
    InvalidArgumentCount {
        call: ExprId,
        range: NodeRange,
        expected: usize,
        actual: usize,
    },
    TypeMismatch {
        expr: ExprId,
        range: NodeRange,
        expected: Ty,
        actual: Ty,
    },
    InvalidExplicitConversion {
        expr: ExprId,
        range: NodeRange,
        from: Ty,
        to: Type,
    },
    InvalidDataLocation {
        range: NodeRange,
        ty: Type,
        location: Location,
    },
    DataLocationRequired {
        range: NodeRange,
        ty: Type,
    },
    UnknownNamedArgument {
        call: ExprId,
        range: NodeRange,
        name: Name,
    },
    DuplicateNamedArgument {
        call: ExprId,
        range: NodeRange,
        name: Name,
    },
    UnresolvedType {
        ty_ref_id: TyNameId,
        range: NodeRange,
        name: Name,
    },
    NoMatchingOverload {
        call: ExprId,
        range: NodeRange,
        candidates: Vec<Callable>,
    },
    AmbiguousOverload {
        call: ExprId,
        range: NodeRange,
        candidates: Vec<Callable>,
    },
    InferenceFailure {
        range: NodeRange,
        message: String,
    },
    StateMutability {
        range: NodeRange,
        declared: Mutability,
        required: Mutability,
        callee: Option<DefId>,
    },
    InheritanceFailure {
        range: NodeRange,
        problems: Vec<String>,
    },
    Redeclaration {
        rejected: DefId,
        range: NodeRange,
        name: Name,
        previous: DefId,
    },
    MissingOverride {
        declaration: DefId,
        range: NodeRange,
        bases: Vec<DefId>,
    },
    UnnecessaryOverride {
        declaration: DefId,
        range: NodeRange,
    },
    NotVirtual {
        declaration: DefId,
        range: NodeRange,
        base: DefId,
    },
    IncompatibleOverride {
        declaration: DefId,
        range: NodeRange,
        base: DefId,
        mismatch: OverrideMismatch,
    },
    UnimplementedOverride {
        declaration: DefId,
        range: NodeRange,
        base: DefId,
    },
    OverrideListMismatch {
        declaration: DefId,
        range: NodeRange,
        expected: Vec<DefId>,
    },
    OverrideConflict {
        contract: DefId,
        range: NodeRange,
        name: Name,
        declarations: Vec<DefId>,
    },
    DuplicateMemberSlot {
        declaration: DefId,
        previous: DefId,
        range: NodeRange,
    },
    InheritedMemberConflict {
        contract: DefId,
        name: Name,
        declarations: Vec<DefId>,
        range: NodeRange,
    },
    UnimplementedContract {
        contract: DefId,
        range: NodeRange,
    },
    MissingVirtual {
        declaration: DefId,
        range: NodeRange,
    },
    MissingImplementation {
        declaration: DefId,
        range: NodeRange,
    },
    VirtualInLibrary {
        declaration: DefId,
        range: NodeRange,
    },
    UnresolvedImport {
        import: ImportId,
        range: NodeRange,
        path: Name,
    },
    ImportSymbolNotFound {
        import: ImportId,
        range: NodeRange,
        name: Name,
        path: Name,
    },
}

impl DiagnosticCode {
    /// Codes anchored at a shared contract name range, where each emission is
    /// already distinct by construction and deduplication by `(code, range)`
    /// would silently drop valid conflicts.
    pub fn allows_multiple(self) -> bool {
        matches!(self, Self::OverrideConflict | Self::InheritedMemberConflict)
    }
}

impl SemanticDiagnostic {

    /// The file a def-map diagnostic belongs to. Inference diagnostics return `None`: their
    /// owner is already file-scoped.
    pub fn declaration_file(&self) -> Option<File> {
        match self {
            Self::Redeclaration { rejected, .. } => Some(rejected.file()),
            Self::DuplicateMemberSlot { declaration, .. } => Some(declaration.file()),
            Self::MissingVirtual { declaration, .. }
            | Self::MissingImplementation { declaration, .. }
            | Self::VirtualInLibrary { declaration, .. } => Some(declaration.file()),
            Self::InheritedMemberConflict { contract, .. } => Some(contract.file()),
            Self::UnresolvedImport { import, .. } | Self::ImportSymbolNotFound { import, .. } => {
                Some(import.file)
            }
            _ => None,
        }
    }
}

/// Accumulates diagnostics with `(code, range)` deduplication — several
/// producers can independently report what collapses to the same fact.
#[derive(Default)]
pub struct DiagnosticSink {
    diagnostics: Vec<SemanticDiagnostic>,
    reported: FxHashSet<DiagnosticKey>,
}

impl DiagnosticSink {
    pub fn push(&mut self, diagnostic: SemanticDiagnostic) {
        if diagnostic.code().allows_multiple()
            || self.reported.insert(diagnostic.key())
        {
            self.diagnostics.push(diagnostic);
        }
    }

    pub fn extend<'a>(&mut self, diagnostics: impl IntoIterator<Item = &'a SemanticDiagnostic>) {
        for diagnostic in diagnostics {
            self.push(diagnostic.clone());
        }
    }

    pub fn finish(self) -> Arc<[SemanticDiagnostic]> {
        self.diagnostics.into()
    }
}
