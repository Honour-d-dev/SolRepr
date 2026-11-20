//! Container membership: which declarations a container may hold.
//!
//! A declaration that cannot belong in its container is filtered where
//! membership is decided, so nothing downstream needs a filter. The container
//! is the owning `DefId`; the member is its `Item`, whose digested facts
//! (`vis`, `kind`, `is_virtual`, `has_body`, `mutability`) answer the rule
//! without reaching into HIR.
//!
//! The table is the conformance record: every row was checked against solc, and
//! two rules a "library holds no variables" reading would have added —
//! `constant` variables and `external` functions — are legal and deliberately
//! absent.
//!
//! Not covered here:
//!
//! - File-level non-constant variables: the grammar accepts only
//!   `constant_variable_declaration` at file level, so `uint x;` never becomes
//!   an item — the rejection is already a parse error, not a rule.
//! - A container nested in a container: the grammar's member walk
//!   drops it, so it never reaches this table. Left to `solc` until the
//!   compiler layer reports.
//! - Struct members: they are not defs, so struct correctness stays in the
//!   inferer.
//! - Rules needing an initializer, the folded member set, or base
//!   linearization (`constant` without a value, "should be marked abstract",
//!   interface bases): those are structural and stay in inference.
//!
//! One violation per declaration: the gate only needs "may this be a member",
//! and solc's secondary complaint on the same declaration is not something a
//! single membership answer can carry.
//!
//! Only the variable rules actually remove the name — see
//! [`Violation::removes_membership`].

use crate::ast::FunctionKind;
use crate::hir::types::{Mutability, VariableKind, Visibility};
use crate::ir::def_map::DefId;
use crate::ir::item_tree::{Function, Item, Modifier, Var};

/// Why a declaration may not be a member of its container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Violation {
    FreeFunctionVisibility,
    FreeFunctionUnimplemented,
    FreeFunctionVirtual,
    InterfaceVariable,
    InterfaceModifier,
    InterfaceUsing,
    InterfaceFunctionVisibility,
    InterfaceFunctionBody,
    InterfaceConstructor,
    LibraryStateVariable,
    LibraryConstructor,
    LibraryFallback,
    LibraryReceive,
    LibraryFunctionVirtual,
    LibraryModifierVirtual,
    LibraryUnimplemented,
    BodylessModifier,
    BodylessFunctionNotVirtual,
    PrivateVirtual,
    ReceiveNotPayable,
}

impl Violation {
    /// Whether the violation also removes the declaration from its container's
    /// namespace.
    ///
    /// Only variables are member candidates, so only their rules change what
    /// resolves. solc still resolves a declaration it rejected — a bodyless
    /// non-virtual function keeps its references and reports them as pointer
    /// or call errors — so the rest are reported at the declaration and stay
    /// reachable, which keeps one invalid declaration from cascading into
    /// "unresolved" errors at every use.
    pub fn removes_membership(self) -> bool {
        matches!(self, Self::InterfaceVariable | Self::LibraryStateVariable)
    }

    /// The rule as a sentence. solc's wording, without the declaration name:
    /// the diagnostic is anchored at the name, so the message need not repeat
    /// it.
    pub fn message(self) -> &'static str {
        match self {
            Self::FreeFunctionVisibility => "free functions cannot have visibility",
            Self::FreeFunctionUnimplemented => "free functions must be implemented",
            Self::FreeFunctionVirtual => "free functions cannot be `virtual`",
            Self::InterfaceVariable => "variables cannot be declared in interfaces",
            Self::InterfaceModifier => "modifiers cannot be defined or declared in interfaces",
            Self::InterfaceUsing => {
                "the `using for` directive is not allowed inside interfaces"
            }
            Self::InterfaceFunctionVisibility => {
                "functions in interfaces must be declared external"
            }
            Self::InterfaceFunctionBody => {
                "functions in interfaces cannot have an implementation"
            }
            Self::InterfaceConstructor => "constructors cannot be defined in interfaces",
            Self::LibraryStateVariable => {
                "a library cannot have non-constant state variables"
            }
            Self::LibraryConstructor => "constructors cannot be defined in libraries",
            Self::LibraryFallback => "libraries cannot have fallback functions",
            Self::LibraryReceive => "libraries cannot have receive ether functions",
            Self::LibraryFunctionVirtual => "library functions cannot be `virtual`",
            Self::LibraryModifierVirtual => "modifiers in a library cannot be `virtual`",
            Self::LibraryUnimplemented => "library functions must be implemented",
            Self::BodylessModifier => {
                "modifiers without implementation must be marked `virtual`"
            }
            Self::BodylessFunctionNotVirtual => {
                "functions without implementation must be marked `virtual`"
            }
            Self::PrivateVirtual => "`virtual` and `private` cannot be used together",
            Self::ReceiveNotPayable => "receive ether functions must be payable",
        }
    }
}

/// `Some(reason)` when `member` may not belong to `container`.
pub fn violation(container: DefId, member: &Item) -> Option<Violation> {
    match (container, member) {
        (DefId::File(_), Item::Function(f)) => file_function(f),
        (DefId::Contract(_), Item::Function(f)) => contract_function(f),
        (DefId::Contract(_), Item::Modifier(m)) => contract_modifier(m),
        (DefId::Interface(_), Item::Var(_)) => Some(Violation::InterfaceVariable),
        (DefId::Interface(_), Item::Modifier(_)) => Some(Violation::InterfaceModifier),
        (DefId::Interface(_), Item::Using(_)) => Some(Violation::InterfaceUsing),
        (DefId::Interface(_), Item::Function(f)) => interface_function(f),
        (DefId::Library(_), Item::Var(v)) => library_var(v),
        (DefId::Library(_), Item::Modifier(m)) => library_modifier(m),
        (DefId::Library(_), Item::Function(f)) => library_function(f),
        _ => None,
    }
}

/// A free function has no visibility, so the *written token* is what is
/// illegal — a defaulted `internal` would hide the violation.
fn file_function(f: &Function) -> Option<Violation> {
    if f.vis.is_some() {
        return Some(Violation::FreeFunctionVisibility);
    }
    // A constructor is a parse error at file level, not a rule this table owns.
    if f.kind == FunctionKind::Regular && !f.has_body {
        return Some(Violation::FreeFunctionUnimplemented);
    }
    f.is_virtual.then_some(Violation::FreeFunctionVirtual)
}

fn contract_function(f: &Function) -> Option<Violation> {
    // A bodyless constructor is the inferer's rule, not this table's.
    if f.kind != FunctionKind::Constructor && !f.has_body && !f.is_virtual {
        return Some(Violation::BodylessFunctionNotVirtual);
    }
    if f.kind == FunctionKind::Receive
        && f.mutability.unwrap_or_default() != Mutability::Payable
    {
        return Some(Violation::ReceiveNotPayable);
    }
    if f.is_virtual && f.vis == Some(Visibility::Private) {
        return Some(Violation::PrivateVirtual);
    }
    None
}

fn contract_modifier(m: &Modifier) -> Option<Violation> {
    // Marking the contract abstract does not rescue a bodyless modifier: solc
    // reports the same on an abstract contract too.
    (!m.has_body && !m.is_virtual).then_some(Violation::BodylessModifier)
}

fn interface_function(f: &Function) -> Option<Violation> {
    match f.kind {
        FunctionKind::Constructor => return Some(Violation::InterfaceConstructor),
        // Both are legal in an interface, `receive` included.
        FunctionKind::Fallback | FunctionKind::Receive => return None,
        FunctionKind::Regular => {}
    }
    if f.has_body {
        return Some(Violation::InterfaceFunctionBody);
    }
    // `function f();` has no visibility at all; either way the written
    // token is not `external`.
    if f.vis != Some(Visibility::External) {
        return Some(Violation::InterfaceFunctionVisibility);
    }
    None
}

fn library_var(v: &Var) -> Option<Violation> {
    (v.kind != VariableKind::Const).then_some(Violation::LibraryStateVariable)
}

fn library_modifier(m: &Modifier) -> Option<Violation> {
    if m.is_virtual {
        return Some(Violation::LibraryModifierVirtual);
    }
    (!m.has_body).then_some(Violation::BodylessModifier)
}

fn library_function(f: &Function) -> Option<Violation> {
    match f.kind {
        FunctionKind::Constructor => return Some(Violation::LibraryConstructor),
        FunctionKind::Fallback => return Some(Violation::LibraryFallback),
        FunctionKind::Receive => return Some(Violation::LibraryReceive),
        FunctionKind::Regular => {}
    }
    if f.is_virtual {
        return Some(Violation::LibraryFunctionVirtual);
    }
    if !f.has_body {
        return Some(Violation::LibraryUnimplemented);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hir::exprs::Name;

    fn function(
        kind: FunctionKind,
        vis: Option<Visibility>,
        mutability: Option<Mutability>,
        is_virtual: bool,
        has_body: bool,
    ) -> Function {
        Function {
            name: Name::from("f"),
            kind,
            vis,
            mutability,
            is_virtual,
            has_body,
        }
    }

    fn var(kind: VariableKind) -> Var {
        Var {
            name: Name::from("v"),
            kind,
            vis: None,
        }
    }

    fn modifier(is_virtual: bool, has_body: bool) -> Modifier {
        Modifier {
            name: Name::from("m"),
            is_virtual,
            has_body,
        }
    }

    const fn regular() -> FunctionKind {
        FunctionKind::Regular
    }

    /// `(case, verdict)` — each verdict is a solc error code the row enforces.
    #[test]
    fn file_rules() {
        assert_eq!(
            file_function(&function(regular(), Some(Visibility::External), None, false, false)),
            Some(Violation::FreeFunctionVisibility)
        );
        assert_eq!(
            file_function(&function(regular(), None, None, false, true)),
            None
        );
        // A free function needs a body, `virtual` does not rescue it.
        assert_eq!(
            file_function(&function(regular(), None, None, false, false)),
            Some(Violation::FreeFunctionUnimplemented)
        );
        assert_eq!(
            file_function(&function(regular(), None, None, true, true)),
            Some(Violation::FreeFunctionVirtual)
        );
    }

    #[test]
    fn contract_rules() {
        // Bodyless needs `virtual`; abstractness is a container fact.
        assert_eq!(
            contract_function(&function(regular(), Some(Visibility::Public), None, false, false)),
            Some(Violation::BodylessFunctionNotVirtual)
        );
        assert_eq!(
            contract_function(&function(regular(), Some(Visibility::Public), None, true, false)),
            None
        );
        assert_eq!(
            contract_function(&function(
                FunctionKind::Receive,
                Some(Visibility::External),
                None,
                false,
                true
            )),
            Some(Violation::ReceiveNotPayable)
        );
        assert_eq!(
            contract_function(&function(
                FunctionKind::Receive,
                Some(Visibility::External),
                Some(Mutability::Payable),
                false,
                true
            )),
            None
        );
        assert_eq!(
            contract_function(&function(
                regular(),
                Some(Visibility::Private),
                None,
                true,
                true
            )),
            Some(Violation::PrivateVirtual)
        );
        // A non-payable fallback is legal.
        assert_eq!(
            contract_function(&function(
                FunctionKind::Fallback,
                Some(Visibility::External),
                None,
                false,
                true
            )),
            None
        );
        // The rule covers every kind: a bodyless fallback must be `virtual` too.
        assert_eq!(
            contract_function(&function(
                FunctionKind::Fallback,
                Some(Visibility::External),
                None,
                false,
                false
            )),
            Some(Violation::BodylessFunctionNotVirtual)
        );
        assert_eq!(
            contract_function(&function(
                FunctionKind::Fallback,
                Some(Visibility::External),
                None,
                true,
                false
            )),
            None
        );
        assert_eq!(
            contract_modifier(&modifier(false, false)),
            Some(Violation::BodylessModifier)
        );
        assert_eq!(contract_modifier(&modifier(true, false)), None);
        assert_eq!(contract_modifier(&modifier(false, true)), None);
    }

    #[test]
    fn interface_rules() {
        assert_eq!(
            interface_function(&function(
                regular(),
                Some(Visibility::External),
                None,
                false,
                false
            )),
            None
        );
        assert_eq!(
            interface_function(&function(regular(), Some(Visibility::Public), None, false, false)),
            Some(Violation::InterfaceFunctionVisibility)
        );
        // No token at all is a violation too.
        assert_eq!(
            interface_function(&function(regular(), None, None, false, false)),
            Some(Violation::InterfaceFunctionVisibility)
        );
        assert_eq!(
            interface_function(&function(
                regular(),
                Some(Visibility::External),
                None,
                false,
                true
            )),
            Some(Violation::InterfaceFunctionBody)
        );
        assert_eq!(
            interface_function(&function(
                FunctionKind::Constructor,
                Some(Visibility::Public),
                None,
                false,
                true
            )),
            Some(Violation::InterfaceConstructor)
        );
        // Both legal, `receive` included.
        for kind in [FunctionKind::Fallback, FunctionKind::Receive] {
            assert_eq!(
                interface_function(&function(kind, Some(Visibility::External), None, false, false)),
                None
            );
        }
    }

    #[test]
    fn library_rules() {
        // `constant` is legal in a library; only non-constant storage is not.
        assert_eq!(library_var(&var(VariableKind::Const)), None);
        assert_eq!(
            library_var(&var(VariableKind::StateVariable)),
            Some(Violation::LibraryStateVariable)
        );
        assert_eq!(
            library_var(&var(VariableKind::Immutable)),
            Some(Violation::LibraryStateVariable)
        );
        for (kind, expected) in [
            (FunctionKind::Constructor, Violation::LibraryConstructor),
            (FunctionKind::Fallback, Violation::LibraryFallback),
            (FunctionKind::Receive, Violation::LibraryReceive),
        ] {
            assert_eq!(
                library_function(&function(kind, Some(Visibility::Public), None, false, true)),
                Some(expected)
            );
        }
        // All four visibilities are legal, `external` included.
        for vis in [
            Visibility::Public,
            Visibility::External,
            Visibility::Internal,
            Visibility::Private,
        ] {
            assert_eq!(
                library_function(&function(regular(), Some(vis), None, false, true)),
                None
            );
        }
        assert_eq!(
            library_function(&function(
                regular(),
                Some(Visibility::Internal),
                None,
                true,
                true
            )),
            Some(Violation::LibraryFunctionVirtual)
        );
        assert_eq!(
            library_function(&function(regular(), Some(Visibility::Public), None, false, false)),
            Some(Violation::LibraryUnimplemented)
        );
        // A modifier's virtual rejection is its own code.
        assert_eq!(
            library_modifier(&modifier(true, true)),
            Some(Violation::LibraryModifierVirtual)
        );
        assert_eq!(
            library_modifier(&modifier(false, false)),
            Some(Violation::BodylessModifier)
        );
        assert_eq!(library_modifier(&modifier(false, true)), None);
    }
}
