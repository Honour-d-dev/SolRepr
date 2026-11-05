use rustc_hash::FxHashSet;
use triomphe::Arc;

use crate::ast::{ImportId, NodeRange};
use crate::hir::access::AccessDenial;
use crate::hir::body_map::LocalId;
use crate::hir::exprs::{ExprId, Name};
use crate::hir::resolution::Callable;
use crate::hir::signatures::OverrideMismatch;
use crate::hir::types::{Location, Mutability, Ty, TyNameId, Type};
use crate::ir::def_map::DefId;
use crate::salsa::File;

/// Where a diagnostic anchors, as an id rather than a position. The layout
/// resolves it to a range at emit, so semantic outputs stay position-free.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DiagnosticAnchor {
    Expr(ExprId),
    TyRef(TyNameId),
    Local(LocalId),
    /// The def's written name — `def_range`.
    Def(DefId),
    /// The whole declaration node.
    Decl(DefId),
    /// The body block of a modifier.
    Body(DefId),
    /// A position with no id — parser diagnostics only.
    Raw(NodeRange),
}

macro_rules! define_diagnostics {
    (
        $(
            $variant:ident {
                $( $field:ident : $ty:ty ),* $(,)?
            } => $anchor:expr
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

            #[allow(unused_variables)]
            pub fn anchor(&self) -> DiagnosticAnchor {
                match self {
                    $(
                        Self::$variant { $( $field, )* .. } => $anchor,
                    )*
                }
            }

            pub fn key(&self) -> DiagnosticKey {
                DiagnosticKey(self.code(), self.anchor())
            }
        }
    };
}

/// Identifies a diagnostic for deduplication. Codes anchored at a shared
/// contract id rather than a unique syntax node opt out via
/// [`DiagnosticCode::allows_multiple`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DiagnosticKey(DiagnosticCode, DiagnosticAnchor);

define_diagnostics! {
    SyntaxError {
        range: NodeRange
    } => DiagnosticAnchor::Raw(*range),
    UnresolvedName {
        expr: ExprId,
        name: Name,
        denials: Vec<AccessDenial>,
    } => DiagnosticAnchor::Expr(*expr),
    UnresolvedMember {
        expr: ExprId,
        receiver: Ty,
        member: Name,
        denials: Vec<AccessDenial>,
    } => DiagnosticAnchor::Expr(*expr),
    // A member reference whose slot holds more than one inherited
    // declaration with no effective winner. `declarations` are the
    // conflicting base declarations.
    AmbiguousMember {
        expr: ExprId,
        name: Name,
        declarations: Vec<DefId>,
    } => DiagnosticAnchor::Expr(*expr),
    InvalidArgumentCount {
        call: ExprId,
        expected: usize,
        actual: usize,
    } => DiagnosticAnchor::Expr(*call),
    TypeMismatch {
        expr: ExprId,
        expected: Ty,
        actual: Ty,
    } => DiagnosticAnchor::Expr(*expr),
    InvalidExplicitConversion {
        expr: ExprId,
        from: Ty,
        to: Type,
    } => DiagnosticAnchor::Expr(*expr),
    InvalidDataLocation {
        ty_ref_id: TyNameId,
        ty: Type,
        location: Location,
    } => DiagnosticAnchor::TyRef(*ty_ref_id),
    DataLocationRequired {
        ty_ref_id: TyNameId,
        ty: Type,
    } => DiagnosticAnchor::TyRef(*ty_ref_id),
    UnknownNamedArgument {
        call: ExprId,
        name: Name,
    } => DiagnosticAnchor::Expr(*call),
    DuplicateNamedArgument {
        call: ExprId,
        name: Name,
    } => DiagnosticAnchor::Expr(*call),
    UnresolvedType {
        ty_ref_id: TyNameId,
        name: Name,
    } => DiagnosticAnchor::TyRef(*ty_ref_id),
    NoMatchingOverload {
        call: ExprId,
        candidates: Vec<Callable>,
    } => DiagnosticAnchor::Expr(*call),
    AmbiguousOverload {
        call: ExprId,
        candidates: Vec<Callable>,
    } => DiagnosticAnchor::Expr(*call),
    InferenceFailure {
        anchor: DiagnosticAnchor,
        message: String,
    } => *anchor,
    StateMutability {
        expr: ExprId,
        declared: Mutability,
        required: Mutability,
        callee: Option<DefId>,
    } => DiagnosticAnchor::Expr(*expr),
    InheritanceFailure {
        contract: DefId,
        problems: Vec<String>,
    } => DiagnosticAnchor::Def(*contract),
    Redeclaration {
        rejected: DefId,
        name: Name,
        previous: DefId,
    } => DiagnosticAnchor::Def(*rejected),
    MissingOverride {
        declaration: DefId,
        bases: Vec<DefId>,
    } => DiagnosticAnchor::Def(*declaration),
    UnnecessaryOverride {
        declaration: DefId,
    } => DiagnosticAnchor::Def(*declaration),
    NotVirtual {
        declaration: DefId,
        base: DefId,
    } => DiagnosticAnchor::Def(*declaration),
    IncompatibleOverride {
        declaration: DefId,
        base: DefId,
        mismatch: OverrideMismatch,
    } => DiagnosticAnchor::Def(*declaration),
    UnimplementedOverride {
        declaration: DefId,
        base: DefId,
    } => DiagnosticAnchor::Def(*declaration),
    OverrideListMismatch {
        declaration: DefId,
        expected: Vec<DefId>,
    } => DiagnosticAnchor::Def(*declaration),
    OverrideConflict {
        contract: DefId,
        name: Name,
        declarations: Vec<DefId>,
    } => DiagnosticAnchor::Def(*contract),
    DuplicateMemberSlot {
        declaration: DefId,
        previous: DefId,
    } => DiagnosticAnchor::Def(*declaration),
    InheritedMemberConflict {
        contract: DefId,
        name: Name,
        declarations: Vec<DefId>,
    } => DiagnosticAnchor::Def(*contract),
    UnimplementedContract {
        contract: DefId,
    } => DiagnosticAnchor::Def(*contract),
    MissingVirtual {
        declaration: DefId,
    } => DiagnosticAnchor::Def(*declaration),
    MissingImplementation {
        declaration: DefId,
    } => DiagnosticAnchor::Def(*declaration),
    VirtualInLibrary {
        declaration: DefId,
    } => DiagnosticAnchor::Def(*declaration),
    UnresolvedImport {
        import: ImportId,
        path: Name,
    } => DiagnosticAnchor::Def(DefId::Import(*import)),
    ImportSymbolNotFound {
        import: ImportId,
        name: Name,
        path: Name,
    } => DiagnosticAnchor::Def(DefId::Import(*import)),
}

impl DiagnosticCode {
    /// Codes anchored at a shared contract id, where each emission is
    /// already distinct by construction and deduplication by
    /// `(code, anchor)` would silently drop valid conflicts.
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

/// Accumulates diagnostics with `(code, anchor)` deduplication — several
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
