//! Expression usage checks (`check_rvalue`, `check_lvalue`, the invocation
//! checks), ambient context (`UsageContext`), lvalue legality, and body
//! state-mutability effect tracking (`Pure < View < NonPayable`).

use crate::ast::{ContractId, FunctionKind};
use crate::hir::body_map::Local as BodyLocal;
use crate::hir::types::{Location, VariableKind};
use crate::hir::builtins;
use crate::hir::exprs::{Expr, ExprId};
use crate::hir::resolution::{CallableOrigin, Field as ResolvedField, Local, Resolution};
use crate::hir::diagnostics::{DiagnosticAnchor, SemanticDiagnostic};
use crate::hir::types::{Mutability, Ty, TyKind, Type};
use crate::ir::def_map::{CallableDefId, DefId};

use super::Inferer;

/// Ambient expression context — which enclosing-use permission is active
/// while a subtree infers. Scoped by [`Inferer::infer_in`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum UsageContext {
    Empty,
    /// The expression is being written to — state reads underneath are
    /// assignment bookkeeping, not `view` effects.
    Lvalue,
    /// Inside `revert`/`require`'s error argument — error invocations are
    /// legal.
    Revert,
    /// Inside a modifier/base-constructor invocation — contract names resolve
    /// to constructor callables.
    Modifier,
    /// Directly in a call's callee — a `f{value: …}` options expression
    /// may stand only here.
    Callee,
}

impl<'db, 'store> Inferer<'db, 'store> {
    /// Infers `expr` with `context` as the ambient [`UsageContext`], then
    /// restores the enclosing context.
    pub(super) fn infer_in(&mut self, expr: ExprId, context: UsageContext) -> Ty {
        let prev = std::mem::replace(&mut self.context, context);
        let ty = self.infer_expr(expr);
        self.context = prev;
        ty
    }

    pub(super) fn in_lvalue(&self) -> bool {
        self.context == UsageContext::Lvalue
    }

    pub(super) fn in_revert(&self) -> bool {
        self.context == UsageContext::Revert
    }

    pub(super) fn in_modifier(&self) -> bool {
        self.context == UsageContext::Modifier
    }

    pub(super) fn in_callee(&self) -> bool {
        self.context == UsageContext::Callee
    }

    /// Whether `base` is an inherited base of the current owner (contract header
    /// or constructor). A regular function may not invoke a base constructor.
    fn allows_base_constructor(&self, base: ContractId) -> bool {
        let container = match self.owner {
            DefId::Contract(_) | DefId::Interface(_) => self.owner,
            DefId::Function(id) if self.db.function_data(id).kind == FunctionKind::Constructor => {
                let Some(owner) = self.db.owner(self.owner) else {
                    return false;
                };
                owner
            }
            _ => return false,
        };
        self.db
            .bases(container)
            .get(1..)
            .is_some_and(|bases| bases.contains(&DefId::Contract(base)))
    }

    /// Infers `expr` as a value and runs the value-position checks: a bare
    /// overload set is not a value, and a tuple hole is not a component.
    pub(super) fn check_rvalue(&mut self, expr: ExprId) -> Ty {
        let ty = self.infer_in(expr, UsageContext::Empty);
        let is_call = matches!(self.input.store.exprs[expr], Expr::Call { .. });
        if !is_call
            && matches!(
                self.tables.expr_resolution(expr),
                Some(Resolution::Callables(_))
            )
        {
            self.fail(
                DiagnosticAnchor::Expr(expr),
                "no matching declaration found after variable lookup",
                (),
            );
        }
        self.check_tuple_holes(expr);
        ty
    }

    /// Infers `expr` as a writable place and runs the lvalue verdict.
    pub(super) fn check_lvalue(&mut self, expr: ExprId) -> Ty {
        let ty = self.infer_in(expr, UsageContext::Lvalue);
        if let Err(reason) = self.lvalue_verdict(expr) {
            self.fail(DiagnosticAnchor::Expr(expr), reason, ());
        }
        ty
    }

    /// Whether `expr` is a writable place — `Err` carries the diagnostic text.
    /// Legality only: the write's *effect* is charged at resolution time.
    fn lvalue_verdict(&self, expr: ExprId) -> Result<(), &'static str> {
        match &self.input.store.exprs[expr] {
            // A name is writable exactly when it denotes a variable.
            Expr::Ident(_) => match self.tables.expr_resolution(expr) {
                Some(Resolution::Local(_)) => Ok(()),
                Some(Resolution::Var(def)) => match def {
                    DefId::Var(id) if self.db.var_data(*id).kind == VariableKind::Const => {
                        Err("constant state cannot be assigned to")
                    }
                    _ => Ok(()),
                },
                _ => Err("expression is not a variable and cannot be assigned to"),
            },
            // A struct field is writable; every other member (a builtin field
            // such as `address.balance`, `array.length`) is not.
            Expr::Member { .. } => match self.tables.expr_resolution(expr) {
                Some(Resolution::Field(ResolvedField::Struct(..))) => Ok(()),
                _ => Err("expression is not a struct field and cannot be assigned to"),
            },
            // An element is writable exactly when its base is.
            Expr::ArrayAccess { base, .. } => self.lvalue_verdict(*base),
            // A destructuring target is writable when every present component
            // is; an omitted component is an ignored slot.
            Expr::Tuple(elems) => elems
                .iter()
                .flatten()
                .try_for_each(|elem| self.lvalue_verdict(*elem)),
            // An inline array materializes a fresh memory array — a writable
            // place (`[1,2,3][0] = 5` compiles, however useless). A slice is
            // never a place, so it stays with the default.
            Expr::InlineArray(_) => Ok(()),
            _ => Err("expression cannot be assigned to"),
        }
    }

    /// Infers a returned value: a value first, then a value of the right type.
    pub(super) fn check_return(&mut self, expr: ExprId) -> Ty {
        let ty = self.check_rvalue(expr);
        self.check_return_value(expr);
        ty
    }

    fn check_return_value(&mut self, expr: ExprId) {
        // `None` means the return types themselves are unresolved; their own
        // diagnostics already fired, so the return statement adds nothing.
        let returns = match self.owner_returns() {
            None => return,
            Some(r) if r.is_empty() => {
                let message = match self.owner {
                    DefId::Modifier(_) => "return arguments not allowed",
                    _ => {
                        "different number of arguments in return statement than in returns declaration"
                    }
                };
                self.fail(DiagnosticAnchor::Expr(expr), message, ());
                return;
            }
            Some(r) => r,
        };

        let value_ty = self.tables.expr_ty(expr);
        if value_ty.is_invalid(self.db) {
            return;
        }
        let components: Vec<Option<Ty>> = match value_ty.kind(self.db) {
            TyKind::Tuple(tuple) => tuple.elements.to_vec(),
            _ => vec![Some(value_ty)],
        };
        if components.len() != returns.len() {
            return self.fail(
                DiagnosticAnchor::Expr(expr),
                "different number of arguments in return statement than in returns declaration",
                (),
            );
        }
        for (component, expected) in components.iter().zip(returns) {
            // A hole cannot be returned; the value-position check reported it.
            let Some(component) = component else { continue };
            let expected = expected.ty();
            if component
                .implicitly_converts_to(expected, self.db)
                .is_none()
            {
                self.report(
                    SemanticDiagnostic::TypeMismatch {
                        expr,
                        expected,
                        actual: *component,
                    },
                    (),
                );
            }
        }
    }

    pub fn owner_returns(&self) -> Option<Box<[Type]>> {
        match self.owner {
            DefId::Function(id) => self.db.function_signature(id).map(|sig| sig.returns),
            // A modifier declares no returns, so a bare `return` is legal and a
            // value is "return arguments not allowed".
            _ => Some(Box::default()),
        }
    }

    /// Infers the callee of an `emit` and checks it denotes an event.
    pub(super) fn check_emit(&mut self, expr: ExprId) -> Ty {
        self.check_invocation(expr, UsageContext::Empty, "an event", |origin| {
            origin.is_event()
        })
    }

    /// Infers the callee of a `revert` and checks it denotes an error.
    pub(super) fn check_revert(&mut self, expr: ExprId) -> Ty {
        self.check_invocation(expr, UsageContext::Revert, "an error", |origin| {
            origin.is_error()
        })
    }

    /// The callee of an invocation that must denote a specific declaration
    /// category — `emit` and `revert`.
    fn check_invocation(
        &mut self,
        expr: ExprId,
        context: UsageContext,
        what: &str,
        want: fn(&CallableOrigin) -> bool,
    ) -> Ty {
        let ty = self.infer_in(expr, context);
        match self.tables.expr_resolution(expr) {
            Some(Resolution::Callable(callable)) if want(callable.origin()) => {}
            Some(Resolution::Callable(_)) => {
                self.fail(DiagnosticAnchor::Expr(expr), format!("this call is not {what}"), ())
            }
            _ => self.fail(DiagnosticAnchor::Expr(expr), "expression is not callable", ()),
        }
        ty
    }

    /// Infers a modifier-position invocation: a modifier, or — in a contract
    /// header or a `constructor` — a base contract's constructor.
    pub(super) fn check_modifier(&mut self, expr: ExprId) -> Ty {
        let ty = self.infer_in(expr, UsageContext::Modifier);
        match self.tables.expr_resolution(expr) {
            Some(Resolution::Callable(callable)) => match callable.origin() {
                CallableOrigin::Def(CallableDefId::Modifier(_)) => {}
                CallableOrigin::Def(CallableDefId::Constructor(base))
                    if self.allows_base_constructor(*base) => {}
                _ => self.fail(DiagnosticAnchor::Expr(expr), "this call is not a modifier", ()),
            },
            _ if self.tables.expr_ty(expr).is_invalid(self.db) => {}
            _ => self.fail(DiagnosticAnchor::Expr(expr), "expression is not callable", ()),
        }
        ty
    }

    fn check_tuple_holes(&mut self, expr: ExprId) {
        let Expr::Tuple(elems) = &self.input.store.exprs[expr] else {
            return;
        };
        if elems.iter().any(Option::is_none) {
            return self.fail(
                DiagnosticAnchor::Expr(expr),
                "tuple component cannot be empty",
                (),
            );
        }
        for elem in elems.iter().flatten() {
            self.check_tuple_holes(*elem);
        }
    }

    


    // ── State effects & payability ───────────────────────────────────────

    /// Records that the site `expr` needs at least `effect`, and reports it
    /// when the owner declared less. This is the one place a body's effect is
    /// widened and the one place it is compared against the declaration.
    pub(super) fn require_effect(
        &mut self,
        expr: ExprId,
        effect: Mutability,
        callee: Option<DefId>,
    ) {
        let effect = effect.state_effect();
        self.effect = self.effect.join(effect);
        let Some(declared) = self.declared_mutability else {
            return;
        };
        if !effect.implicitly_converts_to(declared.state_effect()) {
            self.report(
                SemanticDiagnostic::StateMutability {//@TODO never emited with a callee
                    expr,
                    declared,
                    required: effect,
                    callee,
                },
                (),
            );
        }
    }

    /// `msg.value` reads the call's attached value, which only a `payable`
    /// function may observe. Libraries are exempt — via delegatecall they see
    /// the *calling* function's value.
    pub(super) fn check_msg_value(&mut self, expr: ExprId, obj_ty: Ty, prop: &str) {
        if prop == "value"
            && matches!(obj_ty.kind(self.db), TyKind::Builtin(builtins::BuiltinTy::Msg))
            && !matches!(self.resolver.container(), DefId::Library(_))
            && self.declared_mutability.is_some_and(|mutability| mutability != Mutability::Payable)
        {
            self.fail(
                DiagnosticAnchor::Expr(expr),
                "`msg.value` is only allowed in payable functions",
                (),
            );
        }
    }

    /// A resolved local may only index its own body's arena; a foreign `owner`
    /// is skipped rather than bounds-truncated.
    fn body_local(&self, local: Local) -> Option<&BodyLocal> {
        if self.is_owner(local.owner) {
            return Some(&self.input.locals?[local.id]);
        }
        None
    }

    /// A local's location lives in the body arena, not in its resolution or its
    /// type, so the lookup goes through the owner being inferred — the only
    /// body a resolved local can come from.
    fn is_storage_local(&self, local: Local) -> bool {
        self.body_local(local).is_some_and(|local| {
            local.location(&self.input.store.types).is_some_and(Location::is_storage)
        })
    }

    /// A local's declared kind, looked up through [`Self::body_local`].
    fn local_kind(&self, local: Local) -> Option<VariableKind> {
        self.body_local(local).map(|local| local.kind())
    }

    /// Whether writing through `resolution` mutates storage: a state variable or
    /// a `storage` local. No parameter exemption — a write always counts.
    fn writes_state(&self, resolution: &Resolution) -> bool {
        match resolution {
            Resolution::Var(DefId::Var(id)) => {
                self.db.var_data(*id).kind == VariableKind::StateVariable
            }
            Resolution::Local(local) => self.is_storage_local(*local),
            _ => false,
        }
    }

    /// Whether reading through `resolution` reads state or the environment: a
    /// state variable, a `storage` pointer, `this`, or `msg`/`block`/`tx`.
    fn reads_state(&self, resolution: &Resolution) -> bool {
        match resolution {
            Resolution::Var(DefId::Var(id)) => {
                self.db.var_data(*id).kind == VariableKind::StateVariable
            }
            // A storage-pointer *parameter* does not count as a state read.
            Resolution::Local(local) => {
                self.is_storage_local(*local)
                    && !matches!(self.local_kind(*local), Some(VariableKind::Parameter))
            }
            // `a.balance`/`a.code`/`a.codehash` read the *account* — an
            // environment access independent of `a`'s own location.
            Resolution::Field(ResolvedField::Builtin(field)) => field.reads_env(),
            Resolution::Builtin(object) => object.reads_env(),
            _ => false,
        }
    }

    /// Charges the state/environment effect a resolved name demands: a *write*
    /// under an lvalue root (`s` in `s.x = 1`), a *read* otherwise. Index and
    /// bound positions restore rvalue context first, so the resolution alone
    /// decides the side.
    pub(super) fn check_resolution_mutability(
        &mut self,
        expr: ExprId,
        resolution: &Resolution,
    ) {
        match self.in_lvalue() {
            true if self.writes_state(resolution) => {
                self.require_effect(expr, Mutability::NonPayable, None);
            }
            false if self.reads_state(resolution) => {
                self.require_effect(expr, Mutability::View, None);
            }
            _ => {}
        }
    }
}
