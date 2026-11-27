//! The unrendered form a check emits: a code plus an id anchor, so no position
//! enters a Salsa output. `render` turns these into reportable diagnostics.

use rustc_hash::FxHashSet;
use triomphe::Arc;

use crate::ast::ImportId;
use crate::hir::access::AccessDenial;
use crate::hir::exprs::{ExprId, Name};
use crate::hir::item_data::SemanticId;
use crate::hir::resolution::Callable;
use crate::hir::signatures::OverrideMismatch;
use crate::hir::types::{Location, Mutability, Ty, TyNameId, Type};
use crate::ir::def_map::DefId;
use crate::salsa::File;

/// Where a diagnostic anchors, as an id rather than a position. The layout
/// resolves it to a range at emit, so semantic outputs stay position-free.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DiagnosticAnchor {
    /// The declaration whose store holds the anchor; without a token, the
    /// declaration itself.
    def: DefId,
    /// The exact spelling to anchor at, or `None` for the declaration's written
    /// name (or its whole node when it has none).
    token: Option<SemanticId>,
}

impl DiagnosticAnchor {
    /// The declaration's written name, or its whole node when it has none.
    pub fn def(def: DefId) -> DiagnosticAnchor {
        DiagnosticAnchor { def, token: None }
    }

    /// The token `id` names, read in `def`'s store.
    pub fn token(def: DefId, id: SemanticId) -> DiagnosticAnchor {
        DiagnosticAnchor { def, token: Some(id) }
    }

    pub(crate) fn def_id(self) -> DefId {
        self.def
    }

    pub(crate) fn token_id(self) -> Option<SemanticId> {
        self.token
    }
}

macro_rules! define_diagnostics {
    (
        codes { $( $code:ident ),* $(,)? }
        $(
            $variant:ident {
                $( $field:ident : $ty:ty ),* $(,)?
            } => $anchor:expr
        ),* $(,)?
    ) => {
        #[repr(u8)]
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum DiagnosticCode {
            $( $code, )*
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
    codes { SyntaxError }
    UnresolvedName {
        def: DefId,
        expr: ExprId,
        name: Name,
        denials: Vec<AccessDenial>,
    } => DiagnosticAnchor::token(*def, SemanticId::Expr(*expr)),
    UnresolvedMember {
        def: DefId,
        expr: ExprId,
        receiver: Ty,
        member: Name,
        denials: Vec<AccessDenial>,
    } => DiagnosticAnchor::token(*def, SemanticId::Expr(*expr)),
    // A member reference whose slot holds more than one inherited
    // declaration with no effective winner. `declarations` are the
    // conflicting base declarations.
    AmbiguousMember {
        def: DefId,
        expr: ExprId,
        name: Name,
        declarations: Vec<DefId>,
    } => DiagnosticAnchor::token(*def, SemanticId::Expr(*expr)),
    InvalidArgumentCount {
        def: DefId,
        call: ExprId,
        expected: usize,
        actual: usize,
    } => DiagnosticAnchor::token(*def, SemanticId::Expr(*call)),
    TypeMismatch {
        def: DefId,
        expr: ExprId,
        expected: Ty,
        actual: Ty,
    } => DiagnosticAnchor::token(*def, SemanticId::Expr(*expr)),
    InvalidExplicitConversion {
        def: DefId,
        expr: ExprId,
        from: Ty,
        to: Type,
    } => DiagnosticAnchor::token(*def, SemanticId::Expr(*expr)),
    InvalidDataLocation {
        def: DefId,
        ty_ref_id: TyNameId,
        ty: Type,
        location: Location,
    } => DiagnosticAnchor::token(*def, SemanticId::Type(*ty_ref_id)),
    DataLocationRequired {
        def: DefId,
        ty_ref_id: TyNameId,
        ty: Type,
    } => DiagnosticAnchor::token(*def, SemanticId::Type(*ty_ref_id)),
    UnknownNamedArgument {
        def: DefId,
        call: ExprId,
        name: Name,
    } => DiagnosticAnchor::token(*def, SemanticId::Expr(*call)),
    DuplicateNamedArgument {
        def: DefId,
        call: ExprId,
        name: Name,
    } => DiagnosticAnchor::token(*def, SemanticId::Expr(*call)),
    UnresolvedType {
        def: DefId,
        ty_ref_id: TyNameId,
        name: Name,
    } => DiagnosticAnchor::token(*def, SemanticId::Type(*ty_ref_id)),
    NoMatchingOverload {
        def: DefId,
        call: ExprId,
        candidates: Vec<Callable>,
    } => DiagnosticAnchor::token(*def, SemanticId::Expr(*call)),
    AmbiguousOverload {
        def: DefId,
        call: ExprId,
        candidates: Vec<Callable>,
    } => DiagnosticAnchor::token(*def, SemanticId::Expr(*call)),
    InferenceFailure {
        anchor: DiagnosticAnchor,
        message: String,
    } => *anchor,
    StateMutability {
        def: DefId,
        expr: ExprId,
        declared: Mutability,
        required: Mutability,
        callee: Option<DefId>,
    } => DiagnosticAnchor::token(*def, SemanticId::Expr(*expr)),
    InheritanceFailure {
        contract: DefId,
        problems: Vec<String>,
    } => DiagnosticAnchor::def(*contract),
    Redeclaration {
        rejected: DefId,
        name: Name,
        previous: DefId,
    } => DiagnosticAnchor::def(*rejected),
    MissingOverride {
        declaration: DefId,
        bases: Vec<DefId>,
    } => DiagnosticAnchor::def(*declaration),
    UnnecessaryOverride {
        declaration: DefId,
    } => DiagnosticAnchor::def(*declaration),
    NotVirtual {
        declaration: DefId,
        base: DefId,
    } => DiagnosticAnchor::def(*declaration),
    IncompatibleOverride {
        declaration: DefId,
        base: DefId,
        mismatch: OverrideMismatch,
    } => DiagnosticAnchor::def(*declaration),
    UnimplementedOverride {
        declaration: DefId,
        base: DefId,
    } => DiagnosticAnchor::def(*declaration),
    OverrideListMismatch {
        declaration: DefId,
        expected: Vec<DefId>,
    } => DiagnosticAnchor::def(*declaration),
    OverrideConflict {
        contract: DefId,
        name: Name,
        declarations: Vec<DefId>,
    } => DiagnosticAnchor::def(*contract),
    DuplicateMemberSlot {
        declaration: DefId,
        previous: DefId,
    } => DiagnosticAnchor::def(*declaration),
    InheritedMemberConflict {
        contract: DefId,
        name: Name,
        declarations: Vec<DefId>,
    } => DiagnosticAnchor::def(*contract),
    UnimplementedContract {
        contract: DefId,
    } => DiagnosticAnchor::def(*contract),
    // A declaration that may not be a member of its container. The rule is the
    // table's — see `ir::membership`.
    IllegalMember {
        declaration: DefId,
        container: DefId,
        violation: crate::ir::membership::Violation,
    } => DiagnosticAnchor::def(*declaration),
    UnresolvedImport {
        import: ImportId,
        path: Name,
    } => DiagnosticAnchor::def(DefId::Import(*import)),
    ImportSymbolNotFound {
        import: ImportId,
        name: Name,
        path: Name,
    } => DiagnosticAnchor::def(DefId::Import(*import)),
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
    /// def is already file-scoped.
    pub fn declaration_file(&self) -> Option<File> {
        match self {
            Self::Redeclaration { rejected, .. } => Some(rejected.file()),
            Self::DuplicateMemberSlot { declaration, .. } => Some(declaration.file()),
            Self::IllegalMember { declaration, .. } => Some(declaration.file()),
            Self::InheritedMemberConflict { contract, .. } => Some(contract.file()),
            Self::UnresolvedImport { import, .. } | Self::ImportSymbolNotFound { import, .. } => Some(import.file),
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
        if diagnostic.code().allows_multiple() || self.reported.insert(diagnostic.key()) {
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
