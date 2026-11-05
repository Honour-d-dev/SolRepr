mod call_checker;
mod const_eval;
mod declarations;
mod lower;
mod tables;
mod usage;

use num_bigint::BigInt;
use triomphe::Arc;
pub use tables::Inference;
use const_eval::ConstErrorKind;
use usage::UsageContext;

use crate::ast::NodeRange;
use crate::hir::exprs::{BinaryOp, Expr, ExprId, Name, UnaryOp};
use crate::hir::item_data::VarData;
use crate::hir::syntax_store::OwnerStore;
use crate::hir::resolution::{Field as ResolvedField, Resolution};
use crate::hir::resolver::Resolver;
use crate::hir::semantic_errors::{DiagnosticSink, SemanticDiagnostic};
use crate::hir::types::Primitive::FixedBytes;
use crate::hir::types::{
    Location, Mutability, Primitive, RefKind, Ty, TyKind, Type, VariableKind,
};
use crate::ir::def_map::DefId;
use crate::salsa::HirDb;


/// Early short-circuit on an invalid type.
macro_rules! return_invalid {
    ($ty:expr, $db:expr) => {{
        let ty = $ty;
        if ty.is_invalid($db) {
            return ty;
        }
        ty
    }};
}
pub(crate) use return_invalid;


/// The single inference context: the owner, its name-resolution context, and
/// the accumulated tables. Implementation is split across the sibling
/// namespaces by responsibility, all `impl Inferer`.
pub struct Inferer<'db, 'store> {
    db: &'db dyn HirDb,
    owner: DefId,
    input: OwnerStore<'store>,
    tables: Inference,
    sink: DiagnosticSink,
    resolver: Resolver<'db>,

    /// The mutability the owner declares. A modifier declares none but still
    /// accumulates an effect for the functions that inline it.
    declared_mutability: Option<Mutability>,
    /// Accumulated state-mutability effect (`Pure < View < NonPayable`).
    effect: Mutability,
    /// Ambient usage permission; scoped by `Inferer::infer_in`.
    context: UsageContext,
}

impl<'db, 'store> Inferer<'db, 'store> {
    pub fn run(db: &'db dyn HirDb, owner: DefId, input: OwnerStore<'store>) -> Inference {
        // No source map means a declaration-context query: declaring scopes only.
        let resolver = if input.source_map.is_some() {
            Resolver::build(db, owner)
        } else {
            Resolver::for_declaration(db, owner)
        };
        let mut inferer = Self {
            db,
            owner,
            input,
            tables: Inference::empty(),
            sink: DiagnosticSink::default(),
            resolver,
            declared_mutability: match owner {
                DefId::Function(id) => Some(db.function_data(id).mutability),
                _ => None,
            },
            effect: Mutability::Pure,
            context: UsageContext::Empty,
        };
        inferer.check_definition();
        inferer.infer_all();

        inferer
            .sink
            .extend(inferer.resolver.finish_diagnostics().iter());
        inferer.tables.effect = inferer.effect;
        inferer.tables.diagnostics = inferer.sink.finish();
        inferer.tables
    }

    fn infer_all(&mut self) {
        if let Some(locals) = self.input.locals {
            for (local_id, local) in locals.iter() {
                let ty = self.infer_local(local_id, local.range());
                self.tables.set_local(local_id, ty);
                if let Some(init) = locals[local_id].init() {
                     self.expect_assignable(init, ty);
                }
            }
        }

        if let Some(fields) = self.input.fields {
            for (field_id, field) in fields.iter() {
                let ty = self.infer_field(field_id, field.range);
                self.tables.set_field(field_id, ty);
            }
        }

        if let Some(variants) = self.input.variants {
            for (variant_id, variant) in variants.iter() {
                let ty = self.infer_variant(variant.range);
                self.tables.set_variant(variant_id, ty);
            }
        }

        // Reverse arena order: lowering allocates a child before its parent.
        for (expr_id, _) in self.input.store.exprs.iter().rev() {
            if self.tables.try_expr_ty(expr_id).is_none() {
                self.check_rvalue(expr_id);
            }
        }

        // Types lower last: their locations come from the attached expression.
        for (ty_ref_id, _) in self.input.store.types.iter().rev() {
            self.lower_type(ty_ref_id, false);
        }
    }

    #[inline]
    fn is_owner(&self, def: DefId) -> bool {
        def == self.owner
    }

    /// Can cycle — callers must check [`Self::is_owner`] first.
    fn inference(&self, def: DefId) -> Arc<Inference> {
        self.db.inference(def)
    }

    fn report<R>(&mut self, diagnostic: SemanticDiagnostic, recovery: R) -> R {
        self.sink.push(diagnostic);
        recovery
    }

    fn fail<R>(&mut self, range: NodeRange, message: impl Into<String>, recovery: R) -> R {
        self.report(
            SemanticDiagnostic::InferenceFailure {
                range,
                message: message.into(),
            },
            recovery,
        )
    }

    /// Infers `expr` as a value and asserts it converts to `expected`, poisoning
    /// on failure. Skipped when either side is invalid — it already owns a diagnostic.
    pub(super) fn expect_assignable(&mut self, expr: ExprId, expected: impl Into<Ty>) -> Ty {
        let expected = expected.into();
        let actual = self.check_rvalue(expr);
        if expected.is_invalid(self.db) || actual.is_invalid(self.db) {
            return actual;
        }
        if actual.implicitly_converts_to(expected, self.db).is_none() {
            return self.report(
                SemanticDiagnostic::TypeMismatch {
                    expr,
                    range: self.input.store.expr_ranges[expr],
                    expected,
                    actual,
                },
                Ty::invalid(self.db),
            );
        }
        actual
    }

    // ── Expression resolution ────────────────────────────────────────────

    fn infer_resolution(&mut self, expr: ExprId, resolution: Resolution) -> Ty {
        let range = self.input.store.expr_ranges[expr];
        self.tables.set_expr_resolution(expr, resolution.clone());
        self.check_resolution_mutability(range, &resolution);
        match resolution {
            Resolution::Local(local) => self
                .tables
                .locals
                .get(local.id)
                .copied()
                .unwrap_or_else(|| Type::invalid(self.db))
                .ty(),
            Resolution::Var(var_def) => {
                let data = if let DefId::Var(id) = var_def {
                    self.db.var_data(id)
                } else {
                    return self.fail(
                        range,
                        "variable resolution points to a non-variable",
                        Ty::invalid(self.db),
                    );
                };
                let ty = if self.is_owner(var_def) {
                    // The declaration-site occurrence is the only entry point for a
                    // variable's declaration checks; any other self-resolution
                    // (`uint a = a`) is an illegal self-read.
                    if range == data.range {
                        self.check_var_declaration(&data)
                    } else {
                        return self.fail(
                            range,
                            "a variable's declaration cannot reference the variable itself",
                            Ty::invalid(self.db),
                        );
                    }
                } else {
                    self.inference(var_def).ty(data.ty_ref)
                };
                ty.ty().upcast_from_kind(self.db, data.kind)
            }
            Resolution::Callable(callable) => self.callable_type(&callable),
            Resolution::Callables(_) => Ty::invalid(self.db),
            // An invalid type name stays contagious.
            Resolution::Type(ty) if ty.is_invalid(self.db) => {
                Ty::invalid(self.db)
            }
            Resolution::Type(ty) => Ty::new(self.db, TyKind::Type(ty)),
            Resolution::Module(file) => Ty::new(self.db, TyKind::Module(file)),
            Resolution::MetaType(ty) => Ty::new(self.db, TyKind::Meta(ty)),
            Resolution::Builtin(object) => Ty::new(self.db, TyKind::Builtin(object)),
            Resolution::This(def) => match def {
                DefId::Contract(id) => Ty::new(self.db, TyKind::Contract(id)),
                DefId::Interface(id) => Ty::new(self.db, TyKind::Interface(id)),
                DefId::Library(id) => Ty::new(self.db, TyKind::Library(id)),
                _ => self.fail(
                    range,
                    "`this` does not name a contract, interface, or library",
                    Ty::invalid(self.db),
                ),
            },
            Resolution::Super(def) => match def {
                DefId::Contract(id) => Ty::new(self.db, TyKind::Super(id)),
                _ => self.fail(
                    range,
                    "super resolution is not a contract",
                    Ty::invalid(self.db),
                ),
            },
            Resolution::Literal(literal) => literal.ty(self.db),
            Resolution::Variant(variant) => Ty::new(self.db, TyKind::Enum(variant.enum_id)),
            Resolution::Field(field) => match field {
                ResolvedField::Builtin(builtin) => builtin.ty(self.db).ty(),
                ResolvedField::Struct(owner, field_id) => {
                    let field_ty = if self.is_owner(owner) {
                        let fields = self.input.fields.expect("a struct owner has a field arena");
                        self.lower_type(fields[field_id].ty_ref, false)
                    } else {
                        self.inference(owner).field_ty(field_id)
                    };
                    field_ty.ty()
                }
            },
        }
    }

    /// A variable's declaration checks: initializer assignability, `constant`
    /// folding, and the uninitialized-`constant` error. Returns the declared type.
    fn check_var_declaration(&mut self, data: &VarData) -> Type {
        let declared = self.lower_type(data.ty_ref, false);
        match data.init {
            Some(init) => {
                let init_ty = self.expect_assignable(init, declared);
                if data.kind == VariableKind::Const
                    && let Err(error) = self.eval_const(init)
                {
                    // An untypeable initializer already owns its diagnostic; report
                    // only what the type level cannot see (cycles, fold errors).
                    if !init_ty.is_invalid(self.db)
                        || !matches!(
                            error.kind,
                            ConstErrorKind::NotConstant | ConstErrorKind::Foreign
                        )
                    {
                        self.report_const_error(
                            error,
                            "initial value must be a compile-time constant",
                        );
                    }
                }
            }
            None if data.kind == VariableKind::Const => {
                self.fail(data.range, "Uninitialized \"constant\" variable.", ());
            }
            None => {}
        }
        declared
    }

    /// Types `expr` with no demand of its own, memoized in the tables.
    fn infer_expr(&mut self, expr: ExprId) -> Ty {
        if let Some(result) = self.tables.try_expr_ty(expr) {
            return result;
        }
        let result = self.infer_expr_uncached(expr);
        self.tables.set_expr_ty(expr, result);
        result
    }

    /// Name → resolution → type.
    fn infer_expr_uncached(&mut self, expr: ExprId) -> Ty {
        match self.input.store.exprs[expr].clone() {
            Expr::Ident(name) => {
                let range = self.input.store.expr_ranges[expr];
                let scope = self
                    .input
                    .source_map
                    .and_then(|map| map.expr_scopes.get(expr).copied());
                let resolved = self
                    .resolver
                    .resolve_name_at(&name, scope, range.start, expr, range);
                let Some(resolution) = resolved else {
                    return Ty::invalid(self.db);
                };
                self.infer_resolution(expr, resolution)
            }
            Expr::Literal(literal) => self.infer_resolution(expr, Resolution::Literal(literal)),
            Expr::Binary { op, left, right } => {
                let left_type = return_invalid!(self.infer_expr(left), self.db);
                return_invalid!(self.infer_expr(right), self.db);

                // solc rejects a constant zero divisor everywhere, not just in
                // constant contexts; const eval reports the same failure and the
                // sink dedups it.
                if matches!(op, BinaryOp::Div | BinaryOp::Mod)
                    && self.eval_const(right).is_ok_and(|value| value.is_zero())
                {
                    self.fail(
                        self.input.store.expr_ranges[expr],
                        if matches!(op, BinaryOp::Mod) {
                            "Modulo by zero."
                        } else {
                            "Division by zero."
                        },
                        (),
                    );
                }

                match op {
                    BinaryOp::And
                    | BinaryOp::Or
                    | BinaryOp::Eq
                    | BinaryOp::Ne
                    | BinaryOp::Lt
                    | BinaryOp::Gt
                    | BinaryOp::Le
                    | BinaryOp::Ge => Ty::primitive(self.db, Primitive::Boolean),
                    //@TODO arithmetic result type; the left operand stands in.
                    _ => left_type,
                }
            }
            Expr::Ternary { cond, then, els } => {
                let expected = Ty::primitive(self.db, Primitive::Boolean);
                return_invalid!(self.expect_assignable(cond, expected), self.db);

                let then_ty = return_invalid!(self.infer_expr(then), self.db);
                let else_ty = return_invalid!(self.infer_expr(els), self.db);

                if let Some(common) = Ty::common_ty(self.db, then_ty, else_ty) {
                    common
                } else {
                    self.report(
                        SemanticDiagnostic::TypeMismatch {
                            expr: els,
                            range: self.input.store.expr_ranges[els],
                            expected: then_ty,
                            actual: else_ty,
                        },
                        Ty::invalid(self.db),
                    )
                }
            }
            Expr::Unary { op, arg } => match op {
                // `delete x` resets the place, so it is a write and nothing more.
                UnaryOp::Delete => {
                    self.check_lvalue(arg);
                    Ty::void(self.db)
                }
                UnaryOp::Not => {
                    let expected = Ty::primitive(self.db, Primitive::Boolean);
                    return_invalid!(self.expect_assignable(arg, expected), self.db);
                    expected
                }
                // `-x`/`~x` keep the operand's type; on a literal the operator
                // folds into the constant (`-1` is `int_const -1`).
                UnaryOp::Neg | UnaryOp::BitNot => {
                    let arg_ty = self.check_rvalue(arg);
                    match &self.input.store.exprs[arg] {
                        Expr::Literal(literal) => literal
                            .unary_kind(op)
                            .map(|kind| Ty::new(self.db, TyKind::Literal(kind)))
                            .unwrap_or(arg_ty),
                        _ => arg_ty,
                    }
                }
            },
            Expr::Update { arg, .. } => self.check_lvalue(arg),
            Expr::Assign { lhs, rhs, op: _ } => {
                let lhs_ty = self.check_lvalue(lhs);
                return_invalid!(self.expect_assignable(rhs, lhs_ty), self.db);
                lhs_ty
            }
            Expr::ArrayAccess { base, index } => {
                let base_type = return_invalid!(self.infer_expr(base), self.db);
                match base_type.peel_loc(self.db).kind(self.db) {
                    // Array-type syntax in expression position, e.g. `uint256[]`, in `abi.decode(x, (uint256[]))`
                    TyKind::Type(elem) => {
                        let size = match index {
                            Some(size) => match self.const_size(size) {
                                Some(size) => Some(size),
                                None => return Ty::invalid(self.db),
                            },
                            None => None,
                        };
                        let ty = Type::array(self.db, elem, size);
                        self.infer_resolution(expr, Resolution::Type(ty)) //@TODO infer the type directly
                    }
                    kind => {
                        let Some(index) = index else {
                            return self.fail(
                                self.input.store.expr_ranges[expr],
                                "array access requires an index",
                                Ty::invalid(self.db),
                            );
                        };
                        // The index is rvalue even when the access is in lvalue,
                        // so a state read there still counts.
                        return_invalid!(self.infer_in(index, UsageContext::Empty), self.db);

                        match kind {
                            TyKind::Array { elem, .. } => match base_type.loc(self.db) {
                                Some(loc) => elem.upcast_from(self.db, loc).ty(),
                                None => self.fail(self.input.store.expr_ranges[expr], "array has no data location", Ty::invalid(self.db)),
                            },
                            TyKind::Mapping { value, .. } => value
                                .upcast_from(self.db, Location::Storage(RefKind::Reference))
                                .ty(),
                            TyKind::Primitive(Primitive::Bytes)
                            | TyKind::Primitive(FixedBytes(_)) => {
                                Ty::primitive(self.db, FixedBytes(1))
                            }
                            ty => self.fail(
                                self.input.store.expr_ranges[expr],
                                format!("type `{}` cannot be indexed", ty.display(self.db)),
                                Ty::invalid(self.db),
                            ),
                        }
                    }
                }
            }
            Expr::Slice { base, from, to } => {
                let range = self.input.store.expr_ranges[expr];
                let base_ty = return_invalid!(self.infer_expr(base), self.db);
                // The bounds are checked as `uint256` values —
                // `expect_assignable` already re-establishes rvalue context.
                let uint256 = Ty::primitive(self.db, Primitive::Uint(256));
                for bound in [from, to].into_iter().flatten() {
                    self.expect_assignable(bound, uint256);
                }
                // solc's TypeType check — a type name can't be sliced.
                if matches!(base_ty.kind(self.db), TyKind::Type(_)) {
                    return self.fail(range, "Types cannot be sliced.", Ty::invalid(self.db));
                }
                // A nested slice unwraps to the array it slices.
                let array_ty = match base_ty.kind(self.db) {
                    TyKind::Slice(inner) => inner.ty(),
                    _ => base_ty,
                };
                let kind = array_ty.peel_loc(self.db).kind(self.db);
                match kind {
                    TyKind::Array { .. }
                    | TyKind::Primitive(Primitive::Bytes | Primitive::String) => {}
                    _ => {
                        return self.fail(
                            range,
                            "Index range access is only possible for arrays and array slices.",
                            Ty::invalid(self.db),
                        )
                    }
                }
                if array_ty.loc(self.db) != Some(Location::Calldata) || !kind.is_dynamically_sized() {
                    self.fail(
                        range,
                        "Index range access is only supported for dynamic calldata arrays.",
                        (),
                    );
                } else if let TyKind::Array { elem, .. } = kind
                    && elem.is_dynamically_encoded(self.db)
                {
                    self.fail(
                        range,
                        "Index range access is not supported for arrays with dynamically encoded base types.",
                        (),
                    );
                }
                // solc keeps the slice type even when checks failed; 
                // the "… slice is not implicitly convertible" cascade is intentional.
                let base = array_ty.narrow(self.db).expect("an array type is nameable");
                Ty::new(self.db, TyKind::Slice(base))
            }
            Expr::InlineArray(elems) => {
                let range = self.input.store.expr_ranges[expr];
                // solc's rule: the first element's *mobile type* seeds the
                // deduction, then each later element merges through `common_ty`.
                let mut common: Option<Ty> = None;
                for elem in elems.iter().copied() {
                    let elem_ty = return_invalid!(self.infer_expr(elem), self.db);
                    common = match common {
                        None => Some(elem_ty.mobile_ty(self.db)),
                        Some(acc) => match Ty::common_ty(self.db, acc, elem_ty) {
                            Some(common) => Some(common),
                            None => {
                                return self.fail(
                                    range,
                                    "Unable to deduce common type for array elements.",
                                    Ty::invalid(self.db),
                                )
                            }
                        },
                    };
                }
                let Some(common) = common else {
                    return self.fail(
                        range,
                        "Unable to deduce common type for array elements.",
                        Ty::invalid(self.db),
                    );
                };
                let Some(elem) = common.narrow(self.db) else {
                    return self.fail(
                        range,
                        "Unable to deduce nameable type for array elements. Try adding explicit type conversion for the first element.",
                        Ty::invalid(self.db),
                    );
                };
                let array = Type::array(
                    self.db,
                    elem,
                    Some(BigInt::from(elems.len())),
                );
                array.at_loc(self.db, Location::Memory).ty()
            }
            Expr::Member { obj, prop } => {
                let obj_ty = self.infer_expr(obj);
                let obj_ty = return_invalid!(obj_ty, self.db);
                let range = self.input.store.expr_ranges[expr];
                let resolution = self.resolver.lookup_member_at(obj_ty, &prop, expr, range);
                let Some(resolution) = resolution else {
                    return Ty::invalid(self.db);
                };
                let ty = self.infer_resolution(expr, resolution);
                self.check_msg_value(range, obj_ty, prop.as_str());
                match self.tables.expr_resolution(expr) {
                    Some(Resolution::Field(ResolvedField::Struct(..))) => {
                        let Some(loc) = obj_ty.loc(self.db) else {
                            return self.fail(range, "struct has no data location", Ty::invalid(self.db));
                        };
                        ty.upcast_from(self.db, loc)
                    }
                    _ => ty,
                }
            }
            Expr::MetaType(inner) => {
                // `type(T)` — the inner expression must infer a type prop.
                let inner_ty = self.infer_expr(inner);
                match inner_ty.kind(self.db) {
                    TyKind::Type(name) => self.infer_resolution(expr, Resolution::MetaType(name)),
                    _ => self.fail(
                        self.input.store.expr_ranges[expr],
                        "metatype expression requires a type name",
                        Ty::invalid(self.db),
                    ),
                }
            }
            Expr::New { ty } => {
                let ty_expr_ty = self.infer_expr(ty);
                self.check_new(expr, ty, ty_expr_ty)
            }
            Expr::Call { callee, args } => match self.named_group(&args) {
                Some(named_args) => self.infer_named_call_expr(expr, callee, &named_args),
                None => self.infer_call_expr(expr, callee, &args),
            }
            Expr::ModifierInv(call) => self.check_modifier(call),
            Expr::Emit(call) => self.check_emit(call),
            Expr::Revert(call) => self.check_revert(call),
            Expr::Return(value) => {
                if let Some(value) = value {
                    self.check_return(value);
                } else if self.owner_returns().is_some_and(|returns| !returns.is_empty()) {
                    self.fail(self.input.store.expr_ranges[expr], "return arguments required", ());
                }
                Ty::void(self.db)
            }
            Expr::CallOptions { callee, options } => {
                let callee_ty = self.infer_expr(callee);
                self.check_call_options(expr, callee, callee_ty, &options)
            }
            Expr::Tuple(elems) => {
                let mut components: Vec<Option<Ty>> = Vec::with_capacity(elems.len());
                for elem in elems.iter().copied() {
                    if let Some(elem) = elem {
                        let elem_ty = self.infer_expr(elem);
                        match elem_ty.kind(self.db) {
                            TyKind::Invalid => return Ty::invalid(self.db),
                            TyKind::Void => {
                                return self.fail(
                                    self.input.store.expr_ranges[elem],
                                    "tuple component cannot be empty",
                                    Ty::invalid(self.db),
                                );
                            }
                            _ => components.push(Some(elem_ty)),
                        }
                    } else {
                        components.push(None);
                    };
                }
                match components.as_slice() {
                    // `()` — no components at all.
                    [] => Ty::void(self.db),
                    _ => Ty::tuple_from(self.db, components.into_boxed_slice()),
                }
            }
            Expr::NamedArgs(named) => {
                for (_, value) in named.iter() {
                    self.check_rvalue(*value);
                }
                Ty::void(self.db)
            }
            Expr::Path(path) => {
                let Some(file) = self.db.resolve_to_file(self.owner.file(), &path) else {
                    return self.fail(
                        self.input.store.expr_ranges[expr],
                        format!("unable to resolve path `{path}`"),
                        Ty::invalid(self.db),
                    );
                };
                self.infer_resolution(expr, Resolution::Module(file))
            }
        }
    }

    // ── Call orchestration ───────────────────────────────────────────────

    fn infer_call_expr(&mut self, call: ExprId, callee: ExprId, args: &[ExprId]) -> Ty {
        self.infer_in(callee, UsageContext::Callee);
        if let Some(intrinsic) = self.intrinsic_callee(callee) {
            return self.infer_intrinsic_call_expr(call, intrinsic, args);
        }
        let Some(arg_types) = self.infer_args(args.iter().copied()) else {
            return Ty::invalid(self.db);
        };
        self.check_positional_call(call, callee, args, &arg_types)
    }

    fn infer_named_call_expr(&mut self, call: ExprId, callee: ExprId, args: &[(Name, ExprId)]) -> Ty {
        self.infer_in(callee, UsageContext::Callee);
        if let Some(invalid) = self.check_duplicate_named(call, args) {
            return invalid;
        }
        let Some(arg_types) = self.infer_args(args.iter().map(|(_, value)| *value)) else {
            return Ty::invalid(self.db);
        };
        self.check_named_call(call, callee, args, &arg_types)
    }

    fn infer_args(&mut self, args: impl IntoIterator<Item = ExprId>) -> Option<Vec<Ty>> {
        let iter = args.into_iter();
        let mut arg_types = Vec::with_capacity(iter.size_hint().0);
        for arg in iter {
            let ty = self.check_rvalue(arg);
            if ty.is_invalid(self.db) {
                return None;
            }
            arg_types.push(ty);
        }
        Some(arg_types)
    }
}

