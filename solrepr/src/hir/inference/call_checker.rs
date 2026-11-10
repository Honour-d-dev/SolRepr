use std::borrow::Cow;
use num_bigint::{BigInt, Sign};

use crate::ast::{EnumId, StructId};
use crate::hir::builtins::{BuiltinFn, ParamSchema};
use crate::hir::exprs::{Expr, ExprId, Name, NamedValues};
use crate::hir::syntax_store::SyntaxStore;
use crate::hir::resolution::{Callable, CallableOrigin, Resolution};
use crate::hir::diagnostics::{DiagnosticAnchor, SemanticDiagnostic};
use crate::hir::types::{
    CallKind, Callee, Fn, Location, Mutability, Primitive, Ty, TyKind,
    Type, ViaTypeKind, Visibility, option_type,
};
use crate::ir::def_map::{CallableDefId, DefId, TypeDefId};
use crate::salsa::HirDb;

use super::Inferer;
use super::return_invalid;
use super::usage::UsageContext;

// ── Call selection ───────────────────────────────────────────────────────

/// Why a call could not be typed — a decision, not a diagnostic: the checker
/// decides how to spell it.
enum CallOutcome<'a> {
    Selected(&'a Callable),
    SelectedPtr(&'a Fn<Type>),
    NotInvocable,
    ArityMismatch {
        expected: usize,
        actual: usize,
    },
    ArgumentMismatch(Box<[ArgumentMismatch]>),
    NoMatchingOverload {
        candidates: &'a [Callable],
    },
    AmbiguousOverload {
        candidates: Box<[&'a Callable]>,
    },
    Unusable,
}

/// A usable overload candidate: the declaration, its signature, and the
/// arguments **in that declaration's order**.
type Candidate<'a> = (Callable, Fn<Type>, Cow<'a, [Ty]>);

/// An argument that did not convert to the parameter it was checked against.
struct ArgumentMismatch {
    arg: ExprId,
    expected: Ty,
    actual: Ty,
}

impl<'db, 'store> Inferer<'db, 'store> {
    // ── Pure signature & overload selection ──────────────────────────────

    /// Params not bounded to a receiver/caller.
    /// Attached 'using' functions have exactly one bound
    pub(crate) fn unbound_params<'a>(sig: &'a Fn<Type>, callable: &Callable) -> Option<&'a [Type]> {
        sig.params.get(callable.bound() as usize..)
    }

    /// The parameter argument `index` is checked against; `TypedTail` callables
    /// reuse the last declared parameter. Shared by the argument check and
    /// overload selection so the diagnostic and the choice cannot disagree.
    pub(crate) fn param_for(callable: &Callable, unbound: &[Type], index: usize) -> Option<Ty> {
        let parameter = match unbound.get(index) {
            Some(parameter) => parameter,
            None if matches!(callable.schema(), ParamSchema::TypedTail) => unbound.last()?,
            None => return None,
        };
        Some(parameter.ty())
    }

    /// `Fixed` needs an exact match; `TypedTail`/`AnyTail` a prefix plus tail;
    /// `Intrinsic` arg count belongs to its own checker.
    pub(crate) fn arg_counts_match(sig: &Fn<Type>, callable: &Callable, arg_count: usize) -> bool {
        let Some(unbound) = Self::unbound_params(sig, callable) else {
            return false;
        };
        match callable.schema() {
            ParamSchema::Fixed => arg_count == unbound.len(),
            ParamSchema::TypedTail | ParamSchema::AnyTail => arg_count >= unbound.len(),
            ParamSchema::Intrinsic => true,
        }
    }

    /// Every argument that fails to convert, in order.
    fn arguments_mismatch(
        &self,
        args: &[ExprId],
        arg_types: &[Ty],
        sig: &Fn<Type>,
        callable: &Callable,
    ) -> Vec<ArgumentMismatch> {
        let Some(unbound) = Self::unbound_params(sig, callable) else {
            return Vec::new();
        };
        let mut mismatches = Vec::new();
        for (index, (arg, actual)) in args.iter().zip(arg_types).enumerate() {
            let Some(expected) = Self::param_for(callable, unbound, index) else {
                continue;
            };
            if actual.implicitly_converts_to(expected, self.db).is_none() {
                mismatches.push(ArgumentMismatch {
                    arg: *arg,
                    expected,
                    actual: *actual,
                });
            }
        }
        mismatches
    }

    fn overload_matches<'a>(&self, candidates: &'a [Candidate<'a>]) -> Vec<(&'a Callable, u32)> {
        let mut matches = Vec::new();
        'candidates: for (candidate, sig, arg_types) in candidates {
            if !Self::arg_counts_match(sig, candidate, arg_types.len()) {
                continue;
            }
            let Some(unbound) = Self::unbound_params(sig, candidate) else {
                continue;
            };
            let mut total_cost = 0u32;
            for (index, arg) in arg_types.iter().enumerate() {
                let Some(parameter) = Self::param_for(candidate, unbound, index) else {
                    continue;
                };
                match arg.implicitly_converts_to(parameter, self.db) {
                    Some(cost) => total_cost += cost as u32,
                    None => continue 'candidates,
                }
            }
            matches.push((candidate, total_cost));
        }
        matches
    }

    /// Selects for a call whose single declaration is known; the caller has
    /// already checked invocability.
    fn select_one<'a>(
        &self,
        callable: &'a Callable,
        sig: &'a Fn<Type>,
        args: &'a [ExprId],
        arg_types: &'a [Ty],
    ) -> CallOutcome<'a> {
        let Some(unbound) = Self::unbound_params(sig, callable) else {
            return CallOutcome::Unusable;
        };
        if !Self::arg_counts_match(sig, callable, arg_types.len()) {
            return CallOutcome::ArityMismatch {
                expected: unbound.len(),
                actual: arg_types.len(),
            };
        }
        let mismatches = self.arguments_mismatch(args, arg_types, sig, callable);
        if !mismatches.is_empty() {
            return CallOutcome::ArgumentMismatch(mismatches.into_boxed_slice());
        }
        CallOutcome::Selected(callable)
    }

    /// Selects for a call whose callee is a function pointer: fixed arity, no
    /// bound receiver, and no declaration identity to record.
    fn select_ptr<'a>(
        &self,
        fnty: &'a Fn<Type>,
        args: &[ExprId],
        arg_types: &[Ty],
    ) -> CallOutcome<'a> {
        // A pointer is called at its written type — solc normalizes the
        // *reference* (`this.e` types as `function(bytes memory)`), not the
        // call, so `function(bytes calldata) external p; p(m)` rejects.
        if arg_types.len() != fnty.params.len() {
            return CallOutcome::ArityMismatch {
                expected: fnty.params.len(),
                actual: arg_types.len(),
            };
        }
        let mut mismatches = Vec::new();
        for (index, (arg, actual)) in args.iter().zip(arg_types).enumerate() {
            let expected = fnty.params[index].ty();
            if actual.implicitly_converts_to(expected, self.db).is_none() {
                mismatches.push(ArgumentMismatch {
                    arg: *arg,
                    expected,
                    actual: *actual,
                });
            }
        }
        if mismatches.is_empty() {
            CallOutcome::SelectedPtr(fnty)
        } else {
            CallOutcome::ArgumentMismatch(mismatches.into_boxed_slice())
        }
    }

    /// Selects among overloads. `declared` is the full set as written (for the
    /// "no matching overload" report), `candidates` the usable subset.
    fn select_overloads<'a>(
        &self,
        declared: &'a [Callable],
        candidates: &'a [Candidate<'a>],
    ) -> CallOutcome<'a> {
        if candidates.is_empty() {
            return if declared
                .iter()
                .all(|callable| !callable.is_invocable(self.db))
            {
                CallOutcome::NotInvocable
            } else {
                CallOutcome::Unusable
            };
        }

        let expected = candidates
            .first()
            .and_then(|(callable, sig, _)| Self::unbound_params(sig, callable))
            .map_or(0, |params| params.len());
        if !candidates
            .iter()
            .any(|(callable, sig, types)| Self::arg_counts_match(sig, callable, types.len()))
        {
            return CallOutcome::ArityMismatch {
                expected,
                actual: candidates.first().map_or(0, |(_, _, types)| types.len()),
            };
        }

        let matches = self.overload_matches(candidates);
        if matches.is_empty() {
            return CallOutcome::NoMatchingOverload {
                candidates: declared,
            };
        }
        let best_cost = matches
            .iter()
            .map(|(_, cost)| *cost)
            .min()
            .unwrap_or_default();
        let best = matches
            .into_iter()
            .filter(|(_, cost)| *cost == best_cost)
            .map(|(callable, _)| callable)
            .collect::<Vec<_>>();
        match best.as_slice() {
            &[selected] => CallOutcome::Selected(selected),
            _ => CallOutcome::AmbiguousOverload {
                candidates: best.into_boxed_slice(),
            },
        }
    }

    // ── `new`, call options, and call dispatch ───────────────────────────

    /// Entry point for `Expr::New`: contract creation and dynamic array/bytes
    /// allocation.
    pub(super) fn check_new(&mut self, expr: ExprId, ty: ExprId, ty_expr_ty: Ty) -> Ty {
        return_invalid!(ty_expr_ty, self.db);
        let Some(Resolution::Type(name)) = self.tables.expr_resolution(ty).cloned() else {
            return self.fail(
                DiagnosticAnchor::Expr(expr),
                "`new` requires a type name",
                Ty::invalid(self.db),
            );
        };
        let name = name.peel_loc(self.db);
        let callable = match name.kind(self.db) {
            TyKind::TypeDef(TypeDefId::Contract(id)) => {
                let members = self.db.member_map(DefId::Contract(id));
                if self.db.contract_data(id).is_abstract
                    || members.data.has_unimplemented_members()
                {
                    return self.fail(
                        DiagnosticAnchor::Expr(expr),
                        "Cannot instantiate an abstract contract.",
                        Ty::invalid(self.db),
                    );
                }
                Callable::new_constructor(id)
            }
            // `new T[](n)` allocates: the array type is the identity and
            // its length is the one parameter.
            TyKind::Array { size: None, .. } => Callable::new_allocation(name),
            TyKind::Array { size: Some(_), .. } => {
                return self.fail(
                    DiagnosticAnchor::Expr(expr),
                    "length has to be placed in parentheses after the array type for `new`",
                    Ty::invalid(self.db),
                );
            }
            // `new bytes(n)` / `new string(n)` allocate a dynamic byte
            TyKind::Primitive(Primitive::Bytes | Primitive::String) => {
                Callable::new_allocation(name)
            }
            target => {
                return self.fail(DiagnosticAnchor::Expr(expr), uninstantiable(target), Ty::invalid(self.db));
            }
        };
        self.tables
            .set_expr_resolution(expr, Resolution::Callable(callable));
        self.callable_type(&callable)
    }

    /// Entry point for `Expr::CallOptions`: validates `{value, gas, salt}` and
    /// forwards the callee's type and resolution
    /// The options expression types as the callee itself
    pub(super) fn check_call_options(&mut self, expr: ExprId, callee: ExprId, callee_ty: Ty, options: &NamedValues) -> Ty {
        
        // Carry the callee's identity to the enclosing call expr
        let resolution = self.tables.expr_resolution(callee).cloned();
        if let Some(resolution) = &resolution {
            self.tables.set_expr_resolution(expr, resolution.clone());
        }

        if !self.in_callee() {
            return self.fail(DiagnosticAnchor::Expr(expr), "options can only be set on a function call", callee_ty, );
        }

        if callee_ty.is_invalid(self.db) && !matches!(resolution, Some(Resolution::Callables(_))) {
            return callee_ty;
        }
        // `{…}{…}` reject multiple options
        if matches!(&self.input.store.exprs[callee], Expr::CallOptions { .. }) {
            return self.fail(
                DiagnosticAnchor::Expr(expr),
                "call options have already been set — combine them into a single `{…}`",
                callee_ty,
            );
        }
        // The options the host admits — `None` when it is no option host at all
        let callee_options = match &resolution {
            Some(Resolution::Callable(c)) => c.options(),
            Some(Resolution::Callables(cs)) => cs.iter().find_map(|c| c.options()),
            _ => match callee_ty.kind(self.db) {
                TyKind::FnPtr(sig) => sig.options(),
                _ => None,
            },
        };
        let Some(callee_options) = callee_options else {
            return self.fail(
                DiagnosticAnchor::Expr(expr),
                "function call options can only be set on external function calls or contract creations",
                callee_ty,
            );
        };
        
        let host = match &resolution {
            Some(Resolution::Callable(c)) => match c.builtin() {
                Some(b) => b.name(),
                None if c.origin().is_constructor() => "new",
                None => "function",
            },
            _ => "function",
        };
        
        for (name, value) in options.iter() {
            let expected = match option_type(name) {
                None => self.fail(DiagnosticAnchor::Expr(expr), format!("unknown call option: `{name}`"), None),
                Some(primitive) if callee_options.contains(&name.as_str()) => {
                    Some(Ty::primitive(self.db, primitive))
                }
                Some(_) if name == "salt" => self.fail(
                    DiagnosticAnchor::Expr(expr),
                    "call option: `salt` can only be used with `new`",
                    None,
                ),
                Some(_) => self.fail(
                    DiagnosticAnchor::Expr(expr),
                    format!("call option: `{name}` cannot be used with `{host}`"),
                    None,
                ),
            };

            let actual = self.check_rvalue(*value);
            let Some(expected) = expected else { continue };
            if !actual.is_invalid(self.db)
                && actual.implicitly_converts_to(expected, self.db).is_none()
            {
                self.report(
                    SemanticDiagnostic::TypeMismatch {
                        expr: *value,
                        expected,
                        actual,
                    },
                    (),
                );
            }
        }

        callee_ty
    }

    /// The braced group a call's arguments are, if they are one. The grammar
    /// allows the braced form only as the whole argument list, so it is the
    /// call's single argument when present.
    pub(super) fn named_group(&self, args: &[ExprId]) -> Option<Box<[(Name, ExprId)]>> {
        match args {
            [arg] => match &self.input.store.exprs[*arg] {
                Expr::NamedArgs(named) => Some(named.clone()),
                _ => None,
            },
            _ => None,
        }
    }

    /// If `callee` resolves to an intrinsic (`require`, `abi.decode`,
    /// `abi.encodeCall`), returns that `Callable` so `Inferer` can infer the
    /// intrinsic's arguments contextually instead of eagerly.
    pub(super) fn intrinsic_callee(&self, callee: ExprId) -> Option<Callable> {
        match self.tables.expr_resolution(callee) {
            Some(Resolution::Callable(callable)) if callable.is_intrinsic() => {
                Some(*callable)
            }
            Some(Resolution::Callables(callables))
                if callables.iter().all(Callable::is_intrinsic) =>
            {
                callables.first().cloned()
            }
            _ => None,
        }
    }

    pub(super) fn check_positional_call(
        &mut self,
        expr: ExprId,
        callee: ExprId,
        args: &[ExprId],
        arg_types: &[Ty],
    ) -> Ty {
        let callee_ty = self.tables.expr_ty(callee);
        match self.tables.expr_resolution(callee).cloned() {
            Some(Resolution::Callable(callable)) => {
                self.resolve_callable(expr, callable, args, arg_types)
            }
            Some(Resolution::Callables(callables)) => {
                self.resolve_callables(expr, &callables, arg_types)
            }
            Some(Resolution::Type(ty)) => {
                // Match on the canonical kind, but convert against the located
                // type: a cast's result keeps the argument's location, so the
                // target's default location is what the conversion sees.
                let target = ty.peel_loc(self.db);
                // In modifier/inheritance position (`is Base(args)`,
                // `constructor() Base(args)`), a contract type name denotes
                // that contract's constructor rather than an explicit cast.
                if self.in_modifier()
                    && let TyKind::TypeDef(TypeDefId::Contract(id)) = target.kind(self.db)
                {
                    let callable = Callable::new_constructor(id);
                    return self.resolve_callable(expr, callable, args, arg_types);
                }
                self.tables.set_expr_resolution(expr, Resolution::Type(ty));
                match target.kind(self.db) {
                    TyKind::Invalid => Ty::invalid(self.db),
                    TyKind::TypeDef(TypeDefId::Struct(id)) => {
                        self.check_struct_construction(expr, id, args, arg_types, None)
                    }
                    _ if args.len() == 1 => {
                        let from = arg_types[0];
                        let literal_out_of_range = match target.kind(self.db) {
                            TyKind::TypeDef(TypeDefId::Enum(id)) => {
                                !enum_literal_is_in_range(self.db, self.input.store, args[0], id)
                            }
                            _ => false,
                        };
                        if literal_out_of_range
                            || from.explicitly_converts_to(ty, self.db).is_none()
                        {
                            return self.report(
                                SemanticDiagnostic::InvalidExplicitConversion {
                                    expr,
                                    from,
                                    to: target,
                                },
                                Ty::invalid(self.db),
                            );
                        }
                        // Every cast keeps the *argument's* location 
                        match from.loc(self.db) {
                            Some(loc) => ty.upcast_from(self.db, loc).ty(),
                            None => ty.ty(),
                        }
                    }
                    _ => self.report(
                        SemanticDiagnostic::InvalidArgumentCount {
                            call: expr,
                            expected: 1,
                            actual: args.len(),
                        },
                        Ty::invalid(self.db),
                    ),
                }
            }
            // Other non-valid callee resolutions can still be callable if
            // they're typed as function pointers (e.g. locals, fields).
            _ => match callee_ty.kind(self.db) {
                TyKind::FnPtr(fnty) => {
                    let outcome = self.select_ptr(&fnty, args, arg_types);
                    self.apply_call_outcome(expr, outcome)
                }
                TyKind::Invalid => Ty::invalid(self.db),
                _ => self.fail(DiagnosticAnchor::Expr(expr), "expression is not callable", Ty::invalid(self.db)),
            },
        }
    }

    /// Checks for a duplicate named argument before values are inferred.
    pub(super) fn check_duplicate_named(
        &mut self,
        call: ExprId,
        named: &[(Name, ExprId)],
    ) -> Option<Ty> {
        let name = duplicate_name(named)?;
        Some(self.report(
            SemanticDiagnostic::DuplicateNamedArgument {
                call,
                name: name.clone(),
            },
            Ty::invalid(self.db),
        ))
    }

    /// A call written with named arguments — `f({a: 1, b: 2})`.
    pub(super) fn check_named_call(
        &mut self,
        call: ExprId,
        callee: ExprId,
        named: &[(Name, ExprId)],
        arg_types: &[Ty],
    ) -> Ty {
        let mut names = Vec::with_capacity(named.len());
        let mut values = Vec::with_capacity(named.len());
        for (name, value) in named.iter() {
            names.push(name.clone());
            values.push(*value);
        }

        match self.tables.expr_resolution(callee).cloned() {
            Some(Resolution::Type(ty)) => match ty.peel_loc(self.db).kind(self.db) {
                TyKind::Invalid => {
                    self.unknown_named_argument(call, named.first().map(|(name, _)| name))
                }
                TyKind::TypeDef(TypeDefId::Contract(id)) if self.in_modifier() => {
                    let callable = Callable::new_constructor(id);
                    self.resolve_named_call(call, &callable, named, arg_types)
                }
                TyKind::TypeDef(TypeDefId::Struct(id)) => self.check_struct_construction(
                    call,
                    id,
                    &values,
                    arg_types,
                    Some(&names),
                ),
                // A cast has no parameters to name.
                _ => self.fail(
                    DiagnosticAnchor::Expr(call),
                    "type conversion cannot allow named arguments",
                    Ty::invalid(self.db),
                ),
            },
            Some(Resolution::Callable(callable)) => {
                self.resolve_named_call(call, &callable, named, arg_types)
            }
            Some(Resolution::Callables(callables)) => {
                self.resolve_named_callables(call, &callables, named, arg_types)
            }
            _ => self.unknown_named_argument(call, named.first().map(|(name, _)| name)),
        }
    }

    fn parameter_names(&self, callable: &Callable) -> Option<Box<[Name]>> {
        Some(
            callable.named_parameters(self.db)?
                .iter()
                .map(|(name, _)| name.clone())
                .collect(),
        )
    }

    fn resolve_named_call(
        &mut self,
        call: ExprId,
        callable: &Callable,
        named: &[(Name, ExprId)],
        arg_types: &[Ty],
    ) -> Ty {
        // A variadic builtin has no fixed parameter list to name.
        if matches!(
            callable.schema(),
            ParamSchema::TypedTail | ParamSchema::AnyTail
        ) {
            return self.fail(
                DiagnosticAnchor::Expr(call),
                "named arguments cannot be used for functions that take arbitrary parameters",
                Ty::invalid(self.db),
            );
        }
        let Some(names) = self.parameter_names(callable) else {
            return self.unknown_named_argument(call, named.first().map(|(name, _)| name));
        };

        if named.len() != names.len() {
            return self.report(
                SemanticDiagnostic::InvalidArgumentCount {
                    call,
                    expected: names.len(),
                    actual: named.len(),
                },
                Ty::invalid(self.db),
            );
        }
        if let Some((name, _)) = named.iter().find(|(name, _)| !names.contains(name)) {
            return self.report(
                SemanticDiagnostic::UnknownNamedArgument {
                    call,
                    name: name.clone(),
                },
                Ty::invalid(self.db),
            );
        }
        let (args, types) =
            order_named(&names, named, arg_types).expect("every name matched and the counts agree");
        self.resolve_callable(call, *callable, &args, &types)
    }

    fn resolve_named_callables(
        &mut self,
        call: ExprId,
        callables: &[Callable],
        named: &[(Name, ExprId)],
        arg_types: &[Ty],
    ) -> Ty {
        self.tables.set_expr_resolution(
            call,
            Resolution::Callables(callables.iter().cloned().collect()),
        );
        let matching = callables
            .iter()
            .filter(|callable| callable.is_invocable(self.db))
            .filter(|callable| self.names_match(callable, named))
            .cloned()
            .collect::<Vec<_>>();
        if matching.is_empty() {
            return self.report(
                SemanticDiagnostic::NoMatchingOverload {
                    call,
                    candidates: callables.to_vec(),
                },
                Ty::invalid(self.db),
            );
        }
        self.resolve_overload_candidates(call, callables, &matching, |this, callable| {
            let names = this.parameter_names(callable)?;
            let (_, types) = order_named(&names, named, arg_types)?;
            Some(Cow::Owned(types))
        })
    }

    /// Whether a named call's names are exactly this declaration's parameters.
    fn names_match(&self, callable: &Callable, named: &[(Name, ExprId)]) -> bool {
        self.parameter_names(callable).is_some_and(|names| {
            named.len() == names.len() && named.iter().all(|(name, _)| names.contains(name))
        })
    }

    fn unknown_named_argument(&mut self, call: ExprId, name: Option<&Name>) -> Ty {
        let name = name.map_or(String::new(), Name::to_string);
        self.fail(
            DiagnosticAnchor::Expr(call),
            format!("named argument `{name}` does not match function declaration"),
            Ty::invalid(self.db),
        )
    }

    fn check_struct_construction(
        &mut self,
        call: ExprId,
        id: StructId,
        args: &[ExprId],
        arg_types: &[Ty],
        names: Option<&[Name]>,
    ) -> Ty {
        let data = self.db.struct_data(id);
        let inference = self.db.inference(DefId::Struct(id));
        let fields = data
            .fields
            .iter()
            .map(|(field_id, field)| (field.name.clone(), inference.field_ty(field_id).ty()))
            .collect::<Vec<_>>();

        if args.len() != fields.len() {
            return self.report(
                SemanticDiagnostic::InvalidArgumentCount {
                    call,
                    expected: fields.len(),
                    actual: args.len(),
                },
                Ty::invalid(self.db),
            );
        }
        if let Some(names) = names
            && let Some(name) = names
                .iter()
                .find(|name| !fields.iter().any(|(field, _)| field == *name))
        {
            return self.report(
                SemanticDiagnostic::UnknownNamedArgument {
                    call,
                    name: name.clone(),
                },
                Ty::invalid(self.db),
            );
        }

        for (index, (field_name, field_ty)) in fields.iter().enumerate() {
            let position = match names {
                Some(names) => names
                    .iter()
                    .position(|name| name == field_name)
                    .expect("every name was checked against the fields"),
                None => index,
            };
            let actual = arg_types[position];
            if actual.implicitly_converts_to(*field_ty, self.db).is_none() {
                return self.report(
                    SemanticDiagnostic::TypeMismatch {
                        expr: args[position],
                        expected: *field_ty,
                        actual,
                    },
                    Ty::invalid(self.db),
                );
            }
        }

        Type::new(self.db, TyKind::TypeDef(id.into()))
            .expect("a struct is nameable")
            .ty()
    }

    // ── Call finalization & intrinsics ───────────────────────────────────

    /// The state effect invoking `callable` requires, or `None` when it
    /// constrains nothing (events, errors).
    fn callable_mutability(&self, callable: &Callable) -> Option<Mutability> {
        match callable.origin() {
            CallableOrigin::Def(CallableDefId::Function(id)) => {
                Some(self.db.function_data(*id).mutability)
            }
            CallableOrigin::Def(CallableDefId::Getter(id)) => {
                self.db.getter_signature(*id).map(|sig| sig.mutability)
            }
            // A creation is a state-modifying operation; an allocation is not.
            CallableOrigin::Def(CallableDefId::Constructor(_)) => Some(Mutability::NonPayable),
            CallableOrigin::Allocation(_) => Some(Mutability::Pure),
            CallableOrigin::Builtin(id) => Some(id.mutability()),
            CallableOrigin::Def(CallableDefId::Event(_) | CallableDefId::Error(_)) => None,
            // A modifier declares no mutability of its own, so the effect its
            // body needs is what the function it wraps has to allow.
            CallableOrigin::Def(CallableDefId::Modifier(id)) => {
                let def = DefId::Modifier(*id);
                (!self.is_owner(def)).then(|| self.db.inference(def).effect)
            }
        }
    }

    /// `call`'s callee as a `CallOptions` expression — `(expr, options)`
    /// for the checks that only the selected callee can answer.
    fn call_options(&self, call: ExprId) -> Option<(ExprId, NamedValues)> {
        if let Expr::Call { callee, .. } = &self.input.store.exprs[call]
            && let Expr::CallOptions { options, .. } = &self.input.store.exprs[*callee]
        {
            return Some((*callee, options.clone()));
        }
        None
    }

    /// Whether `callable` can receive a `value` option — the selected member's
    /// payability. `None` for kinds `value` cannot reach; the externality check
    /// owns their diagnostics.
    fn accepts_value(&self, callable: &Callable) -> Option<bool> {
        let mutability = match callable.origin() {
            CallableOrigin::Def(CallableDefId::Function(id)) => {
                Some(self.db.function_data(*id).mutability)
            }
            CallableOrigin::Def(CallableDefId::Getter(id)) => {
                self.db.getter_signature(*id).map(|sig| sig.mutability)
            }
            CallableOrigin::Def(CallableDefId::Constructor(id)) => Some(
                self.db
                    .contract_data(*id)
                    .constructor
                    .map(|ctor| self.db.function_data(ctor).mutability)
                    .unwrap_or(Mutability::NonPayable),
            ),
            CallableOrigin::Builtin(builtin) => Some(builtin.mutability()),
            CallableOrigin::Allocation(_)
            | CallableOrigin::Def(
                CallableDefId::Error(_) | CallableDefId::Event(_) | CallableDefId::Modifier(_),
            ) => None,
        };
        mutability.map(|mutability| mutability == Mutability::Payable)
    }

    fn finish_call(&mut self, call: ExprId, callable: &Callable) -> Ty {
        self.tables
            .set_expr_resolution(call, Resolution::Callable(Callable::clone(callable)));
        if callable.origin().is_error() && !self.in_revert() {
            return self.fail(
                DiagnosticAnchor::Expr(call),
                "errors can only be used with revert statements or require functions",
                Ty::invalid(self.db),
            );
        }
        if let Some(effect) = self.callable_mutability(callable) {
            self.require_effect(call, effect, callable.def());
        }
        // Call options defer the checks that need the selected member: on an
        // overload set the picked member must be external (or a `new` creation),
        // and `value` attaches only to a payable target.
        if let Some((options_expr, options)) = self.call_options(call) {
            let external = callable.is_external()
                || callable.origin().is_constructor();
            if matches!(
                self.tables.expr_resolution(options_expr),
                Some(Resolution::Callables(_))
            ) && !external
            {
                return self.fail(
                    DiagnosticAnchor::Expr(options_expr),
                    "function call options can only be set on external function calls or contract creations",
                    Ty::invalid(self.db),
                );
            }
            if external
                && options.iter().any(|(name, _)| name.as_str() == "value")
                // A builtin's option set already decided `value` at the
                // options node — `call` is the only member that admits it.
                && callable.builtin().is_none()
                && self.accepts_value(callable) == Some(false)
            {
                return self.fail(
                    DiagnosticAnchor::Expr(options_expr),
                    "cannot set option `value` on a non-payable function",
                    Ty::invalid(self.db),
                );
            }
        }
        self.callable_sig(callable, call)
            .map(|sig| sig.result(self.db))
            .unwrap_or_else(|| Ty::invalid(self.db))
    }

    /// Dispatches an intrinsic call to its finisher: `require`,
    /// `abi.decode`, `abi.encodeCall`. The arguments infer contextually —
    /// `require`'s optional message is an error-construction position.
    pub(super) fn infer_intrinsic_call_expr(
        &mut self,
        call: ExprId,
        callable: Callable,
        args: &[ExprId],
    ) -> Ty {
        self.tables
            .set_expr_resolution(call, Resolution::Callable(callable));
        // An intrinsic bypasses selection, but not the effect lattice.
        if let Some(effect) = self.callable_mutability(&callable) {
            self.require_effect(call, effect, callable.def());
        }
        let Some(builtin) = callable.builtin() else {
            return self.fail(
                DiagnosticAnchor::Expr(call),
                "intrinsic call has no builtin identity",
                Ty::invalid(self.db),
            );
        };
        match builtin {
            BuiltinFn::Require => {
                if let Some(invalid) = self.check_intrinsic_arity(call, args.len(), &[1, 2], 2) {
                    return invalid;
                }
                let condition =
                    self.expect_assignable(args[0], Ty::primitive(self.db, Primitive::Boolean));
                let message_ty = if let [_, message] = args {
                    let actual = self.infer_in(*message, UsageContext::Revert);
                    Some(return_invalid!(actual, self.db))
                } else {
                    None
                };
                return_invalid!(condition, self.db);
                self.finish_require_call(args, message_ty)
            }
            BuiltinFn::Decode => {
                if let Some(invalid) = self.check_intrinsic_arity(call, args.len(), &[2], 2) {
                    return invalid;
                }
                let data =
                    self.expect_assignable(args[0], Ty::primitive(self.db, Primitive::Bytes));
                return_invalid!(self.check_rvalue(args[1]), self.db);
                return_invalid!(data, self.db);
                self.finish_abi_decode_call(args)
            }
            BuiltinFn::EncodeCall => {
                if let Some(invalid) = self.check_intrinsic_arity(call, args.len(), &[2], 2) {
                    return invalid;
                }
                let target_ty = return_invalid!(self.check_rvalue(args[0]), self.db);
                let Some(params) = self.encode_call_target_params(call, args[0], target_ty)
                else {
                    return Ty::invalid(self.db);
                };
                return_invalid!(self.check_rvalue(args[1]), self.db);
                self.finish_abi_encode_call(call, args, &params)
            }
            _ => self.fail(
                DiagnosticAnchor::Expr(call),
                format!("no intrinsic checker for `{}`", builtin.name()),
                Ty::invalid(self.db),
            ),
        }
    }

    fn resolve_callable(
        &mut self,
        call: ExprId,
        callable: Callable,
        args: &[ExprId],
        arg_types: &[Ty],
    ) -> Ty {
        self.tables
            .set_expr_resolution(call, Resolution::Callable(callable));
        if !callable.is_invocable(self.db) {
            return self.apply_call_outcome(call, CallOutcome::NotInvocable);
        }
        let Some(sig) = self.callable_sig(&callable, call) else {
            return Ty::invalid(self.db);
        };
        let sig = if callable.encodes_args(&sig) {
            sig.external_abi(self.db)
        } else {
            sig
        };
        let outcome = self.select_one(&callable, &sig, args, arg_types);
        self.apply_call_outcome(call, outcome)
    }

    fn resolve_callables(&mut self, call: ExprId, callables: &[Callable], arg_types: &[Ty]) -> Ty {
        self.tables.set_expr_resolution(
            call,
            Resolution::Callables(callables.iter().cloned().collect()),
        );
        self.resolve_overload_candidates(call, callables, callables, |_, _| {
            Some(Cow::Borrowed(arg_types))
        })
    }

    fn resolve_overload_candidates<'a>(
        &mut self,
        call: ExprId,
        declared: &[Callable],
        viable: &[Callable],
        args_for: impl std::ops::Fn(&Self, &Callable) -> Option<Cow<'a, [Ty]>>,
    ) -> Ty {
        let mut invocable = viable
            .iter()
            .filter(|callable| callable.is_invocable(self.db));
        let candidates = invocable
            .clone()
            .filter_map(|callable| {
                // An unavailable signature only rules this candidate out —
                // the diagnostic is deferred until no candidate survives.
                let sig = self.try_callable_sig(callable).ok()?;
                let sig = if callable.encodes_args(&sig) {
                    sig.external_abi(self.db)
                } else {
                    sig
                };
                Self::unbound_params(&sig, callable)?;
                Some((*callable, sig, args_for(self, callable)?))
            })
            .collect::<Vec<_>>();
        if candidates.is_empty()
            && let Some(unavailable) =
                invocable.find(|callable| self.try_callable_sig(callable).is_err())
        {
            self.callable_sig(unavailable, call);
            return Ty::invalid(self.db);
        }
        let outcome = self.select_overloads(declared, &candidates);
        self.apply_call_outcome(call, outcome)
    }

    /// Turns a selection into the call's result and the diagnostic it implies —
    /// the one place call failures are spelled.
    fn apply_call_outcome(&mut self, call: ExprId, outcome: CallOutcome<'_>) -> Ty {
        match outcome {
            CallOutcome::Selected(callable) => self.finish_call(call, callable),
            CallOutcome::SelectedPtr(fnty) => {
                // A pointer call bypasses `finish_call` — there is no declaration
                // to select — but a `value` option still needs a payable target.
                self.require_effect(call, fnty.mutability, None);
                if fnty.mutability != Mutability::Payable
                    && let Some((options_expr, options)) = self.call_options(call)
                    && options.iter().any(|(name, _)| name.as_str() == "value")
                {
                    return self.fail(
                        DiagnosticAnchor::Expr(options_expr),
                        "cannot set option `value` on a non-payable function",
                        Ty::invalid(self.db),
                    );
                }
                fnty.result(self.db)
            }
            CallOutcome::NotInvocable => self.fail(
                DiagnosticAnchor::Expr(call),
                "declaration-only function cannot be called",
                Ty::invalid(self.db),
            ),
            CallOutcome::ArityMismatch { expected, actual } => self.report(
                SemanticDiagnostic::InvalidArgumentCount {
                    call,
                    expected,
                    actual,
                },
                Ty::invalid(self.db),
            ),
            CallOutcome::ArgumentMismatch(mismatches) => {
                for mismatch in mismatches {
                    self.report(
                        SemanticDiagnostic::TypeMismatch {
                            expr: mismatch.arg,
                            expected: mismatch.expected,
                            actual: mismatch.actual,
                        },
                        (),
                    );
                }
                Ty::invalid(self.db)
            }
            CallOutcome::NoMatchingOverload { candidates } => self.report(
                SemanticDiagnostic::NoMatchingOverload {
                    call,
                    candidates: candidates.to_vec(),
                },
                Ty::invalid(self.db),
            ),
            CallOutcome::AmbiguousOverload { candidates } => {
                let tied: Box<[Callable]> = candidates
                    .iter()
                    .map(|callable| **callable)
                    .collect();
                self.tables
                    .set_expr_resolution(call, Resolution::Callables(tied.clone()));
                self.report(
                    SemanticDiagnostic::AmbiguousOverload {
                        call,
                        candidates: tied.into_vec(),
                    },
                    Ty::invalid(self.db),
                )
            }
            CallOutcome::Unusable => Ty::invalid(self.db),
        }
    }

    pub(super) fn check_intrinsic_arity(
        &mut self,
        call: ExprId,
        actual: usize,
        valid: &[usize],
        expected: usize,
    ) -> Option<Ty> {
        if valid.contains(&actual) {
            return None;
        }
        Some(self.report(
            SemanticDiagnostic::InvalidArgumentCount {
                call,
                expected,
                actual,
            },
            Ty::invalid(self.db),
        ))
    }

    /// `require(bool)` / `require(bool, string)` / `require(bool, E(...))`.
    fn finish_require_call(&mut self, args: &[ExprId], message_ty: Option<Ty>) -> Ty {
        if let (Some(actual), [_, message]) = (message_ty, args) {
            let is_error_call = matches!(actual.kind(self.db), TyKind::Void)
                && matches!(
                    self.tables.expr_resolution(*message),
                    Some(Resolution::Callable(callee))
                        if matches!(callee.def(), Some(DefId::Error(_)))
                );
            if !is_error_call {
                let expected = Ty::primitive(self.db, Primitive::String);
                if actual.implicitly_converts_to(expected, self.db).is_none() {
                    return self.report(
                        SemanticDiagnostic::TypeMismatch {
                            expr: *message,
                            expected,
                            actual,
                        },
                        Ty::invalid(self.db),
                    );
                }
            }
        }
        // `require` returns nothing — valueless, like `emit`.
        Ty::new(self.db, TyKind::Void)
    }

    /// `abi.decode(data, (T1, T2, ..))`
    fn finish_abi_decode_call(&mut self, args: &[ExprId]) -> Ty {

        let names = match &self.input.store.exprs[args[1]] {
            Expr::Tuple(elems) => {
                let mut names = Vec::with_capacity(elems.len());
                for elem in elems.iter() {
                    let Some(elem) = elem else {
                        return self.fail(
                            DiagnosticAnchor::Expr(args[1]),
                            "tuple component cannot be empty",
                            Ty::invalid(self.db),
                        );
                    };
                    let TyKind::Type(name) = self.tables.expr_ty(*elem).kind(self.db) else {
                        return self.fail(
                            DiagnosticAnchor::Expr(args[1]),
                            "argument has to be a type name",
                            Ty::invalid(self.db),
                        );
                    };
                    names.push(name);
                }
                names
            }
            _ => match self.tables.expr_ty(args[1]).kind(self.db) {
                TyKind::Type(name) => vec![name],
                _ => {
                    return self.fail(
                        DiagnosticAnchor::Expr(args[1]),
                        "the second argument to `abi.decode` has to be a tuple of types",
                        Ty::invalid(self.db),
                    );
                }
            },
        };
        // A single type is its own result; an empty list has none.
        Ty::from_types(self.db, &names)
    }

    /// `abi.encodeCall(f, (args…))` — the first argument must denote a function
    /// and the second a tuple whose components convert to that function's
    /// parameters.
    pub(super) fn encode_call_target_params(
        &mut self,
        call: ExprId,
        target_expr: ExprId,
        target_ty: Ty,
    ) -> Option<Box<[Type]>> {
        match self.tables.expr_resolution(target_expr).cloned() {
            Some(Resolution::Callable(target)) => {
                // solc: "Cannot use library functions for abi.encodeCall."
                if matches!(target.kind(), CallKind::ViaType(ViaTypeKind::Library)) {
                    return self.fail(
                        DiagnosticAnchor::Expr(call),
                        "Cannot use library functions for abi.encodeCall.",
                        None,
                    );
                }
                let sig = self.callable_sig(&target, call)?;
                // "Expected regular external function type, or external view on
                // public function": `this.f`/`c.f` are external views, `C.f` is
                // the external function type only for an `external` declaration.
                if !target.is_external() && sig.visibility != Visibility::External {
                    return self.fail(
                        DiagnosticAnchor::Expr(call),
                        "Expected regular external function type, or external view on public function.",
                        None,
                    );
                }
                // `encodeCall` is itself an ABI boundary: the arguments are
                // encoded exactly as an external call's would be.
                Some(sig.external_abi(self.db).params)
            }
            _ => match target_ty.kind(self.db) {
                TyKind::FnPtr(sig) if sig.visibility == Visibility::External => {
                    Some(sig.external_abi(self.db).params)
                }
                TyKind::FnPtr(_) => self.fail(
                    DiagnosticAnchor::Expr(call),
                    "Expected regular external function type, or external view on public function.",
                    None,
                ),
                _ => self.fail(
                    DiagnosticAnchor::Expr(call),
                    "The first argument of abi.encodeCall must be a function pointer.",
                    None,
                ),
            },
        }
    }

    fn finish_abi_encode_call(
        &mut self,
        call: ExprId,
        args: &[ExprId],
        params: &[Type],
    ) -> Ty {
        // `(x)` is a parenthesized expression, not a one-component tuple, so a
        // lone argument is its own component.
        let components: Box<[Option<ExprId>]> = match &self.input.store.exprs[args[1]] {
            Expr::Tuple(elems) => elems.clone(),
            _ => Box::from([Some(args[1])]),
        };
        if components.len() != params.len() {
            return self.report(
                SemanticDiagnostic::InvalidArgumentCount {
                    call,
                    expected: params.len(),
                    actual: components.len(),
                },
                Ty::invalid(self.db),
            );
        }
        for (component, expected) in components.iter().zip(params.iter()) {
            let Some(component) = component else {
                return self.fail(
                    DiagnosticAnchor::Expr(args[1]),
                    "tuple component cannot be empty",
                    Ty::invalid(self.db),
                );
            };
            let actual = self.tables.expr_ty(*component);
            if actual.implicitly_converts_to(*expected, self.db).is_none() {
                return self.report(
                    SemanticDiagnostic::TypeMismatch {
                        expr: *component,
                        expected: expected.ty(),
                        actual,
                    },
                    Ty::invalid(self.db),
                );
            }
        }
        // `abi.encodeCall` yields a `bytes memory`, like every ABI encoder.
        Type::primitive(self.db, Primitive::Bytes).ty()
    }

    // ── Signatures & callable types ──────────────────────────────────────

    fn try_callable_sig(&self, callable: &Callable) -> Result<Fn<Type>, &'static str> {
        match callable.origin() {
            CallableOrigin::Def(CallableDefId::Function(id)) => self
                .db
                .function_signature(*id)
                .ok_or("function signature is unavailable"),
            CallableOrigin::Def(CallableDefId::Constructor(id)) => {
                let mut sig = match self.db.contract_data(*id).constructor {
                    Some(ctor) => self
                        .db
                        .function_signature(ctor)
                        .ok_or("constructor signature is unavailable")?,
                    None => Fn::default(),
                };
                sig.returns = Box::from([
                    Type::new(self.db, TyKind::TypeDef((*id).into()))
                        .expect("a contract is nameable")
                ]);
                Ok(sig)
            }
            CallableOrigin::Allocation(ty) => Ok(Fn {
                visibility: Visibility::Internal,
                mutability: Mutability::Pure,
                params: Box::from([Type::primitive(self.db, Primitive::Uint(256))]),
                returns: Box::from([ty.at_loc(self.db, Location::Memory)]),
            }),
            CallableOrigin::Def(CallableDefId::Event(id)) => self
                .db
                .event_signature(*id)
                .ok_or("event signature is unavailable"),
            CallableOrigin::Def(CallableDefId::Error(id)) => self
                .db
                .error_signature(*id)
                .ok_or("error signature is unavailable"),
            CallableOrigin::Def(CallableDefId::Modifier(id)) => self
                .db
                .modifier_signature(*id)
                .ok_or("modifier signature is unavailable"),
            CallableOrigin::Builtin(builtin) => Ok(builtin.call_sig(self.db)),
            CallableOrigin::Def(CallableDefId::Getter(id)) => self
                .db
                .getter_signature(*id)
                .ok_or("getter variable type is unavailable"),
        }
    }

    /// [`Self::try_callable_sig`], reporting when the signature is unavailable.
    fn callable_sig(&mut self, callable: &Callable, call: ExprId) -> Option<Fn<Type>> {
        match self.try_callable_sig(callable) {
            Ok(sig) => Some(sig),
            Err(message) => self.fail(DiagnosticAnchor::Expr(call), message, None),
        }
    }

    pub(super) fn callable_type(&self, callable: &Callable) -> Ty {
        match callable.origin() {
            CallableOrigin::Builtin(builtin) => Ty::new(
                self.db,
                TyKind::Function {
                    callee: Callee::Builtin(*builtin),
                    kind: callable.kind(),
                },
            ),
            CallableOrigin::Def(CallableDefId::Function(id)) => Ty::new(
                self.db,
                TyKind::Function {
                    callee: Callee::Fn(*id),
                    kind: callable.kind(),
                },
            ),
            CallableOrigin::Def(CallableDefId::Getter(id)) => Ty::new(
                self.db,
                TyKind::Function {
                    callee: Callee::Getter(*id),
                    kind: callable.kind(),
                },
            ),
            CallableOrigin::Def(CallableDefId::Constructor(id)) => Ty::new(
                self.db,
                TyKind::Function {
                    callee: Callee::Constructor(*id),
                    kind: callable.kind(),
                },
            ),
            CallableOrigin::Allocation(ty) => Ty::new(
                self.db,
                TyKind::Function {
                    callee: Callee::Allocation(*ty),
                    kind: callable.kind(),
                },
            ),
            // The rest drop their kinds. On Resolution-level kind decodes callability
            // On Type-level Kind decodes call-semantics which isn't apllicabe to them
            CallableOrigin::Def(CallableDefId::Error(id)) => Ty::new(self.db, TyKind::Error(*id)),
            CallableOrigin::Def(CallableDefId::Event(id)) => Ty::new(self.db, TyKind::Event(*id)),
            CallableOrigin::Def(CallableDefId::Modifier(id)) => {
                Ty::new(self.db, TyKind::Modifier(*id))
            }
        }
    }
}

// ── Enums ────────────────────────────────────────────────────────────────

fn enum_literal_is_in_range(
    db: &dyn HirDb,
    store: &SyntaxStore,
    arg: ExprId,
    id: EnumId,
) -> bool {
    let value = match &store.exprs[arg] {
        Expr::Literal(literal) => literal.integer_value(),
        _ => None,
    };
    let Some(value) = value else {
        return true;
    };
    value.sign() != Sign::Minus && value < BigInt::from(db.enum_data(id).variants.len())
}

// ── Named arguments ──────────────────────────────────────────────────────

/// The first name given twice, if any.
fn duplicate_name(named: &[(Name, ExprId)]) -> Option<&Name> {
    (0..named.len())
        .find(|&index| {
            let name = &named[index].0;
            named[..index].iter().any(|(seen, _)| seen == name)
        })
        .map(|index| &named[index].0)
}

fn order_named(
    names: &[Name],
    named: &[(Name, ExprId)],
    arg_types: &[Ty],
) -> Option<(Vec<ExprId>, Vec<Ty>)> {
    let ordered = names
        .iter()
        .map(|param| {
            named
                .iter()
                .zip(arg_types)
                .find(|((name, _), _)| name == param)
                .map(|((_, value), ty)| (*value, *ty))
        })
        .collect::<Option<Vec<_>>>()?;
    Some(ordered.into_iter().unzip())
}

fn uninstantiable(kind: TyKind) -> &'static str {
    match kind {
        TyKind::TypeDef(TypeDefId::Interface(_)) => "Cannot instantiate an interface.",
        TyKind::TypeDef(TypeDefId::Library(_)) => "Invalid use of a library name.",
        // A primitive is not a declaration at all; solc names what it wanted.
        TyKind::Primitive(_) => "Contract or array type expected.",
        _ => "Identifier is not a contract.",
    }
}
