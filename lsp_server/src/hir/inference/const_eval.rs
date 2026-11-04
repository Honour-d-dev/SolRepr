use num_bigint::BigInt;

use crate::ast::NodeRange;
use crate::hir::body_map::BodySourceMap;
use crate::hir::types::VariableKind;
use crate::hir::exprs::{BinaryOp, Expr, ExprId, Literal, Name, UnaryOp};
use crate::hir::syntax_store::SyntaxStore;
use crate::hir::resolution::{CallableOrigin, Resolution};
use crate::hir::resolver::Resolver;
use crate::hir::types::{Primitive, TyKind, Type};
use crate::ir::def_map::{DefId, DefNamespace, ScopeData};
use crate::salsa::hir_db::def_name;
use crate::salsa::{FileId, HirDb};

use super::Inferer;

// ── Values ───────────────────────────────────────────────────────────────

/// A folded compile-time value. Integer and rational arithmetic is exact;
/// an integer is a rational with `den == 1`.
#[derive(Clone)]
pub(super) enum ConstValue {
    /// `num / den` — `den > 0` always. Integer literals, enum indices, and
    /// addresses-as-numbers live here; non-integral literals keep their
    /// fraction so `1/2*4` is `2`.
    Rational(BigInt, BigInt),
    Bool(bool),
    /// `bytes`, `bytesN`, `hex"…"`, and address values.
    Bytes(Box<[u8]>),
    /// A `string` literal or `type(C).name`.
    String(Box<str>),
    /// A legal constant whose value this evaluator does not compute —
    /// we do not compute hash/math builtins, `type(I).interfaceId`,
    /// `type(C).*Code`. It is a verdict ("valid in a constant context").
    Opaque,
}

impl ConstValue {
    fn int(value: impl Into<BigInt>) -> Self {
        Self::Rational(value.into(), BigInt::from(1u8))
    }

    pub(super) fn is_zero(&self) -> bool {
        matches!(self, Self::Rational(n, _) if *n == BigInt::from(0u8))
    }

    fn rational(&self) -> Option<(BigInt, BigInt)> {
        match self {
            Self::Rational(num, den) => Some((num.clone(), den.clone())),
            _ => None,
        }
    }

    /// The exact integer value — `None` for non-integral or non-numeric values.
    fn as_int(&self) -> Option<BigInt> {
        match self {
            Self::Rational(num, den) if num % den == BigInt::from(0u8) => Some(num / den),
            _ => None,
        }
    }

    /// The value as a machine `usize` — non-negative integral rationals only.
    pub(super) fn as_usize(&self) -> Option<usize> {
        self.as_int().and_then(|value| value.try_into().ok())
    }

    fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    /// The UTF-8 content for byte-level operations (`.length`, `b[i]`,
    /// `bytesN` conversions).
    fn as_bytes(&self) -> Option<Vec<u8>> {
        match self {
            Self::Bytes(bytes) => Some(bytes.to_vec()),
            Self::String(text) => Some(text.as_bytes().to_vec()),
            _ => None,
        }
    }
}

fn rat(num: BigInt, den: BigInt) -> ConstValue {
    if den < BigInt::from(0u8) {
        ConstValue::Rational(-num, -den)
    } else {
        ConstValue::Rational(num, den)
    }
}

fn two_pow(bits: u32) -> BigInt {
    BigInt::from(1u8) << bits
}

/// `value mod 2^bits` — the low `bits` of the two's-complement value, so
/// negatives wrap exactly like the runtime conversion.
fn low_bits(value: &BigInt, bits: u32) -> BigInt {
    value & (two_pow(bits) - BigInt::from(1u8))
}

/// `value` as a big-endian byte string of exactly `width` bytes — the low
/// `width * 8` bits, zero-padded on the left.
fn to_be_bytes(value: &BigInt, width: usize) -> Box<[u8]> {
    let unsigned = low_bits(value, (width * 8) as u32);
    let (_, bytes) = unsigned.to_bytes_be();
    let mut out = vec![0u8; width];
    let keep = bytes.len().min(width);
    out[width - keep..].copy_from_slice(&bytes[bytes.len() - keep..]);
    out.into_boxed_slice()
}

// ── Errors ───────────────────────────────────────────────────────────────

/// Why an expression does not fold to a [`ConstValue`]. `range` always
/// points into the *caller's* store: failures inside a foreign constant's
/// initializer re-anchor to the reference that reached it.
pub(super) struct ConstError {
    pub range: NodeRange,
    pub kind: ConstErrorKind,
}

pub(super) enum ConstErrorKind {
    /// No constant denotes this expression — an unresolved name, a call, an
    /// `immutable`, `msg.sender`. Reported with the call site's own phrasing.
    NotConstant,
    /// `const A = B; const B = A;` — `via` is the constant the cycle
    /// re-enters, named in the diagnostic. Propagates across foreign stores
    /// because every reference on the cycle is a reporting site.
    Cyclic(DefId),
    /// `x / 0` or `x % 0`. Inference's own divisor check reports the same
    /// failure at the same range — the sink dedups it.
    ZeroDivision { modulo: bool },
    /// A folded value violates a bound the type level cannot see — an
    /// out-of-range enum index, a negative array length. Always reported.
    Eval(String),
    /// The failure belongs to another declaration's initializer — that
    /// declaration's own const-init check owns the diagnostic.
    Foreign,
}

// ── Inferer entry points ─────────────────────────────────────────────────

impl<'db, 'store> Inferer<'db, 'store> {
    pub(super) fn eval_const(&self, expr: ExprId) -> Result<ConstValue, ConstError> {
        ConstEval::new(self.db, self.input.store, &self.resolver, self.input.source_map).eval(expr)
    }

    /// `eval_const` for consumers needing the folded integer — array lengths.
    /// `None` means no positive integer; the diagnostic is reported here.
    pub(super) fn const_size(&mut self, expr: ExprId) -> Option<BigInt> {
        let range = self
            .input
            .store
            .expr_ranges
            .get(expr)
            .copied()
            .unwrap_or(NodeRange { start: 0, end: 0 });
        match self.eval_const(expr) {
            // solc keeps the length as `u256` — positive and < 2^256.
            Ok(value) => match value
                .as_int()
                .filter(|size| *size > BigInt::from(0u8) && *size < two_pow(256))
            {
                Some(size) => Some(size),
                // An opaque constant can't size the array — the type
                // lowers without one, but it is not an error.
                None if matches!(value, ConstValue::Opaque) => None,
                None => self.fail(range, "invalid array length", None),
            },
            Err(error) => {
                self.report_const_error(error, "array size must be a constant expression");
                None
            }
        }
    }

    /// Reports a `ConstError` at the site `error` names — `not_constant` is
    /// the caller's phrasing for the `NotConstant` kind. `Foreign` errors
    /// stay silent: the failing declaration's own check owns them.
    pub(super) fn report_const_error(&mut self, error: ConstError, not_constant: &str) {
        let message = match &error.kind {
            ConstErrorKind::Foreign => return,
            ConstErrorKind::NotConstant => not_constant.into(),
            ConstErrorKind::Cyclic(via) => format!(
                "The value of the constant {} has a cyclic dependency.",
                def_name(self.db, *via)
            ),
            ConstErrorKind::ZeroDivision { modulo } => {
                if *modulo { "Modulo by zero." } else { "Division by zero." }.into()
            }
            ConstErrorKind::Eval(message) => message.clone(),
        };
        self.fail(error.range, message, ());
    }
}

// ── The evaluator ────────────────────────────────────────────────────────

/// Evaluates compile-time-constant expressions, tracking visited declarations
/// to break cycles. Separate from `Inferer` because a foreign constant's
/// initializer evaluates in *its* store, resolver, and scopes.
pub(super) struct ConstEval<'a, 'db> {
    db: &'db dyn HirDb,
    store: &'a SyntaxStore,
    resolver: &'a Resolver<'db>,
    source_map: Option<&'a BodySourceMap>,
    visiting: Vec<DefId>,
}

impl<'a, 'db> ConstEval<'a, 'db> {
    pub(super) fn new(
        db: &'db dyn HirDb,
        store: &'a SyntaxStore,
        resolver: &'a Resolver<'db>,
        source_map: Option<&'a BodySourceMap>,
    ) -> Self {
        Self {
            db,
            store,
            resolver,
            source_map,
            visiting: Vec::new(),
        }
    }

    fn range(&self, expr: ExprId) -> NodeRange {
        self.store
            .expr_ranges
            .get(expr)
            .copied()
            .unwrap_or(NodeRange { start: 0, end: 0 })
    }

    fn not_constant(&self, expr: ExprId) -> ConstError {
        ConstError {
            range: self.range(expr),
            kind: ConstErrorKind::NotConstant,
        }
    }

    fn eval_error(&self, expr: ExprId, message: impl Into<String>) -> ConstError {
        ConstError {
            range: self.range(expr),
            kind: ConstErrorKind::Eval(message.into()),
        }
    }
    fn resolve_ident(&self, expr: ExprId, name: &Name) -> Option<Resolution> {
        let scope = self
            .source_map
            .and_then(|map| map.expr_scopes.get(expr).copied());
        let offset = self
            .store
            .expr_ranges
            .get(expr)
            .map(|range| range.start)
            .unwrap_or_default();
        self.resolver.resolve_name(name, scope, offset)
    }

    pub(super) fn eval(&mut self, expr: ExprId) -> Result<ConstValue, ConstError> {
        match self.store.exprs[expr].clone() {
            Expr::Literal(literal) => self.eval_literal(expr, &literal),
            Expr::Ident(name) => match self.resolve_ident(expr, &name) {
                Some(Resolution::Var(def)) => self.eval_var(expr, def),
                Some(Resolution::Variant(variant)) => Ok(ConstValue::int(u32::from(
                    variant.id.into_raw(),
                ))),
                _ => Err(self.not_constant(expr)),
            },
            Expr::Binary { op, left, right } => self.eval_binary(expr, op, left, right),
            Expr::Unary { op, arg } => {
                let value = self.eval(arg)?;
                // An opaque operand yields an opaque result — still a legal
                // constant, just unevaluated.
                if matches!(value, ConstValue::Opaque) && !matches!(op, UnaryOp::Delete) {
                    return Ok(ConstValue::Opaque);
                }
                match op {
                    UnaryOp::Neg => value
                        .rational()
                        .map(|(num, den)| rat(-num, den))
                        .ok_or_else(|| self.not_constant(expr)),
                    UnaryOp::Not => value
                        .as_bool()
                        .map(|value| ConstValue::Bool(!value))
                        .ok_or_else(|| self.not_constant(expr)),
                    UnaryOp::BitNot => value
                        .as_int()
                        .map(|value| ConstValue::int(-(value + 1u8)))
                        .ok_or_else(|| self.not_constant(expr)),
                    UnaryOp::Delete => Err(self.not_constant(expr)),
                }
            }
            Expr::Ternary { cond, then, els } => {
                let cond = self.eval(cond)?;
                match cond.as_bool() {
                    Some(true) => self.eval(then),
                    Some(false) => self.eval(els),
                    _ if matches!(cond, ConstValue::Opaque) => {
                        // No branch is selectable, but both still must be
                        // legal constants — evaluate for diagnostics.
                        self.eval(then)?;
                        self.eval(els)?;
                        Ok(ConstValue::Opaque)
                    }
                    _ => Err(self.not_constant(expr)),
                }
            }
            Expr::Member { obj, prop } => self.eval_member(expr, obj, &prop),
            Expr::Call { callee, args } => self.eval_call(expr, callee, &args),
            Expr::ArrayAccess {
                base,
                index: Some(index),
            } => self.eval_index(expr, base, index),
            // `(x)` lowers as a single-element tuple — peel it like solc's
            // parens. Real tuples are not foldable values.
            Expr::Tuple(elems) => match &elems[..] {
                [Some(inner)] => self.eval(*inner),
                _ => Err(self.not_constant(expr)),
            },
            _ => Err(self.not_constant(expr)),
        }
    }

    fn eval_literal(&self, expr: ExprId, literal: &Literal) -> Result<ConstValue, ConstError> {
        match literal {
            Literal::Boolean(value) => Ok(ConstValue::Bool(*value)),
            Literal::Number(_) => literal
                .rational_value()
                .map(|(num, den)| rat(num, den))
                .ok_or_else(|| self.not_constant(expr)),
            Literal::String(_) => match literal.decoded_bytes() {
                Some(bytes) => Ok(match String::from_utf8(bytes.clone()) {
                    Ok(text) => ConstValue::String(text.into()),
                    Err(_) => ConstValue::Bytes(bytes.into()),
                }),
                None => Err(self.not_constant(expr)),
            },
            Literal::HexString(_) => literal
                .decoded_bytes()
                .map(|bytes| ConstValue::Bytes(bytes.into()))
                .ok_or_else(|| self.not_constant(expr)),
        }
    }

    fn eval_binary(
        &mut self,
        expr: ExprId,
        op: BinaryOp,
        left: ExprId,
        right: ExprId,
    ) -> Result<ConstValue, ConstError> {
        let left = self.eval(left)?;
        let right = self.eval(right)?;
        // `x / 0` errors whatever `x` is — even an opaque constant.
        if matches!(op, BinaryOp::Div | BinaryOp::Mod) && right.is_zero() {
            return Err(ConstError {
                range: self.range(expr),
                kind: ConstErrorKind::ZeroDivision {
                    modulo: matches!(op, BinaryOp::Mod),
                },
            });
        }
        // An opaque operand yields an opaque result — still a legal
        // constant, just unevaluated.
        if matches!(left, ConstValue::Opaque) || matches!(right, ConstValue::Opaque) {
            return Ok(ConstValue::Opaque);
        }
        match op {
            BinaryOp::And | BinaryOp::Or => {
                let (left, right) = match (left.as_bool(), right.as_bool()) {
                    (Some(left), Some(right)) => (left, right),
                    _ => return Err(self.not_constant(expr)),
                };
                Ok(ConstValue::Bool(match op {
                    BinaryOp::And => left && right,
                    _ => left || right,
                }))
            }
            BinaryOp::Eq | BinaryOp::Ne => {
                let equal = self.values_equal(&left, &right).ok_or_else(|| self.not_constant(expr))?;
                Ok(ConstValue::Bool(matches!(op, BinaryOp::Eq) == equal))
            }
            BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
                let (l_num, l_den) = left.rational().ok_or_else(|| self.not_constant(expr))?;
                let (r_num, r_den) = right.rational().ok_or_else(|| self.not_constant(expr))?;
                let ordering = (l_num * &r_den).cmp(&(r_num * l_den));
                Ok(ConstValue::Bool(match op {
                    BinaryOp::Lt => ordering.is_lt(),
                    BinaryOp::Le => ordering.is_le(),
                    BinaryOp::Gt => ordering.is_gt(),
                    _ => ordering.is_ge(),
                }))
            }
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div => {
                let (l_num, l_den) = left.rational().ok_or_else(|| self.not_constant(expr))?;
                let (r_num, r_den) = right.rational().ok_or_else(|| self.not_constant(expr))?;
                Ok(match op {
                    BinaryOp::Add => rat(l_num * &r_den + r_num * &l_den, l_den * r_den),
                    BinaryOp::Sub => rat(l_num * &r_den - r_num * &l_den, l_den * r_den),
                    BinaryOp::Mul => rat(l_num * r_num, l_den * r_den),
                    _ => rat(l_num * r_den, l_den * r_num),
                })
            }
            BinaryOp::Mod => {
                let left = left.as_int().ok_or_else(|| self.not_constant(expr))?;
                let right = right.as_int().ok_or_else(|| self.not_constant(expr))?;
                Ok(ConstValue::int(left % right))
            }
            BinaryOp::Pow => {
                let (num, den) = left.rational().ok_or_else(|| self.not_constant(expr))?;
                let exponent = right.as_int().ok_or_else(|| self.not_constant(expr))?;
                let exponent = i64::try_from(&exponent)
                    .map_err(|_| self.eval_error(expr, "exponent too large"))?;
                let unsigned = u32::try_from(exponent.unsigned_abs())
                    .map_err(|_| self.eval_error(expr, "exponent too large"))?;
                if exponent >= 0 {
                    Ok(rat(num.pow(unsigned), den.pow(unsigned)))
                } else if num == BigInt::from(0u8) {
                    Err(ConstError {
                        range: self.range(expr),
                        kind: ConstErrorKind::ZeroDivision { modulo: false },
                    })
                } else {
                    Ok(rat(den.pow(unsigned), num.pow(unsigned)))
                }
            }
            BinaryOp::Shl | BinaryOp::Shr => {
                let value = left.as_int().ok_or_else(|| self.not_constant(expr))?;
                let shift = right.as_int().ok_or_else(|| self.not_constant(expr))?;
                let shift = u32::try_from(&shift).ok().map(|shift| {
                    match op {
                        BinaryOp::Shl => value << shift,
                        _ => value >> shift,
                    }
                });
                shift
                    .map(ConstValue::int)
                    .ok_or_else(|| self.eval_error(expr, "invalid shift amount"))
            }
            BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor => {
                let left = left.as_int().ok_or_else(|| self.not_constant(expr))?;
                let right = right.as_int().ok_or_else(|| self.not_constant(expr))?;
                Ok(ConstValue::int(match op {
                    BinaryOp::BitAnd => left & right,
                    BinaryOp::BitOr => left | right,
                    _ => left ^ right,
                }))
            }
        }
    }

    fn values_equal(&self, left: &ConstValue, right: &ConstValue) -> Option<bool> {
        match (left, right) {
            (ConstValue::Bool(a), ConstValue::Bool(b)) => Some(a == b),
            (ConstValue::Bytes(a), ConstValue::Bytes(b)) => Some(a == b),
            (ConstValue::String(a), ConstValue::String(b)) => Some(a == b),
            _ => {
                let (l_num, l_den) = left.rational()?;
                let (r_num, r_den) = right.rational()?;
                Some((l_num * r_den).cmp(&(r_num * l_den)).is_eq())
            }
        }
    }

    fn eval_member(
        &mut self,
        expr: ExprId,
        obj: ExprId,
        prop: &Name,
    ) -> Result<ConstValue, ConstError> {
        if let Expr::MetaType(inner) = self.store.exprs[obj] {
            return self.eval_meta(expr, inner, prop);
        }
        match self.namespace_ref(obj) {
            Some(Resolution::Type(ty)) => self.type_member(expr, &ty, prop),
            Some(Resolution::Module(file)) => self.file_member(expr, file, prop),
            // `.length` is the only value member solc folds — and only on
            // `bytes`, never `string`.
            _ if prop == "length" => match self.eval(obj)? {
                ConstValue::Bytes(bytes) => Ok(ConstValue::int(bytes.len())),
                ConstValue::Opaque => Ok(ConstValue::Opaque),
                _ => Err(self.not_constant(expr)),
            },
            _ => Err(self.not_constant(expr)),
        }
    }

    /// `T(x)` — the only const-evaluable call: a conversion of a folded argument.
    /// Foldable builtins (`keccak256`, `addmod`, …) yield `Opaque`, but their
    /// arguments still evaluate so nested non-constants report.
    fn eval_call(
        &mut self,
        expr: ExprId,
        callee: ExprId,
        args: &[ExprId],
    ) -> Result<ConstValue, ConstError> {
        match self.namespace_ref(callee) {
            Some(Resolution::Type(ty)) => {
                let [arg] = args else {
                    return Err(self.not_constant(expr));
                };
                if matches!(self.store.exprs[*arg], Expr::NamedArgs(_)) {
                    return Err(self.not_constant(expr));
                }
                let value = self.eval(*arg)?;
                self.convert(expr, &ty, value)
            }
            _ => {
                for arg in args {
                    if matches!(self.store.exprs[*arg], Expr::NamedArgs(_)) {
                        return Err(self.not_constant(expr));
                    }
                    self.eval(*arg)?;
                }
                if self.is_foldable_builtin(callee) {
                    Ok(ConstValue::Opaque)
                } else {
                    Err(self.not_constant(expr))
                }
            }
        }
    }

    /// A builtin function solc folds in constant expressions. Checked
    /// through resolution so a user-defined `keccak256` shadow stays
    /// `NotConstant` — it is genuinely not foldable.
    fn is_foldable_builtin(&self, callee: ExprId) -> bool {
        let Expr::Ident(name) = &self.store.exprs[callee] else {
            return false;
        };
        matches!(
            self.resolve_ident(callee, name),
            Some(Resolution::Callable(callable))
                if matches!(callable.origin(), CallableOrigin::Builtin(builtin)
                    if builtin.is_const())
        )
    }

    /// `b[i]` on a constant `bytes` value.
    fn eval_index(
        &mut self,
        expr: ExprId,
        base: ExprId,
        index: ExprId,
    ) -> Result<ConstValue, ConstError> {
        let value = self.eval(base)?;
        let index = self.eval(index)?;
        if matches!(value, ConstValue::Opaque) || matches!(index, ConstValue::Opaque) {
            return Ok(ConstValue::Opaque);
        }
        let index = index.as_usize().ok_or_else(|| self.not_constant(expr))?;
        match value {
            ConstValue::Bytes(bytes) => bytes
                .get(index)
                .map(|byte| ConstValue::int(*byte))
                .ok_or_else(|| self.eval_error(expr, "index out of range")),
            _ => Err(self.not_constant(expr)),
        }
    }

    /// `type(T).min` / `.max` / `.name`. `.interfaceId` and `.*Code` are
    /// `Opaque` — solc computes them but no consumer needs the bytes. On the
    /// wrong receiver kind they are plain `NotConstant` errors.
    fn eval_meta(
        &mut self,
        expr: ExprId,
        inner: ExprId,
        prop: &Name,
    ) -> Result<ConstValue, ConstError> {
        let Some(Resolution::Type(ty)) = self.namespace_ref(inner) else {
            return Err(self.not_constant(expr));
        };
        let bits = |width: u16| -> BigInt { two_pow(u32::from(width)) };
        match (ty.kind(self.db), prop.as_str()) {
            (TyKind::Primitive(Primitive::Uint(_)), "min") => Ok(ConstValue::int(0u8)),
            (TyKind::Primitive(Primitive::Uint(n)), "max") => {
                Ok(ConstValue::int(bits(n) - BigInt::from(1u8)))
            }
            (TyKind::Primitive(Primitive::Int(n)), "min") => {
                Ok(ConstValue::int(-bits(n - 1)))
            }
            (TyKind::Primitive(Primitive::Int(n)), "max") => {
                Ok(ConstValue::int(bits(n - 1) - BigInt::from(1u8)))
            }
            (TyKind::Enum(_), "min") => Ok(ConstValue::int(0u8)),
            (TyKind::Enum(id), "max") => {
                let variants = self.db.enum_data(id).variants.len();
                if variants == 0 {
                    return Err(self.not_constant(expr));
                }
                Ok(ConstValue::int(variants - 1))
            }
            (kind, "name") => match kind.def_id() {
                Some(def @ (DefId::Contract(_) | DefId::Interface(_) | DefId::Library(_))) => {
                    Ok(ConstValue::String(def_name(self.db, def).into()))
                }
                _ => Err(self.not_constant(expr)),
            },
            (TyKind::Interface(_), "interfaceId") => Ok(ConstValue::Opaque),
            (TyKind::Contract(_), "creationCode" | "runtimeCode") => Ok(ConstValue::Opaque),
            _ => Err(self.not_constant(expr)),
        }
    }

    /// A member on a type namespace — an enum variant (`E.X`), or a contract
    /// constant (`C.K`). Contract-type member access is declared-only in
    /// solc: `D.K` does not reach constants `D` inherits.
    fn type_member(
        &mut self,
        expr: ExprId,
        ty: &Type,
        prop: &Name,
    ) -> Result<ConstValue, ConstError> {
        match ty.kind(self.db) {
            TyKind::Enum(id) => {
                let data = self.db.enum_data(id);
                data.variants
                    .iter()
                    .position(|(_, variant)| variant.name == *prop)
                    .map(ConstValue::int)
                    .ok_or_else(|| self.not_constant(expr))
            }
            kind => {
                let Some(def) = kind.def_id().filter(|def| {
                    matches!(
                        *def,
                        DefId::Contract(_) | DefId::Interface(_) | DefId::Library(_)
                    )
                }) else {
                    return Err(self.not_constant(expr));
                };
                match self.scope_member(&def, prop) {
                    Some(Resolution::Var(def)) => self.eval_var(expr, def),
                    _ => Err(self.not_constant(expr)),
                }
            }
        }
    }

    /// `prop` on a file module — a file-scope constant (`lib.K`), through
    /// the file's own scope and its imported scopes.
    fn file_member(
        &mut self,
        expr: ExprId,
        file: FileId,
        prop: &Name,
    ) -> Result<ConstValue, ConstError> {
        match self.file_member_ref(file, prop) {
            Some(Resolution::Var(def)) => self.eval_var(expr, def),
            _ => Err(self.not_constant(expr)),
        }
    }

    /// Resolution of an expression used on the left of `.` — a bare name,
    /// an import path, or a deeper member path (`C.E`). Consumers match the
    /// subset of `Resolution` that can be a path prefix.
    fn namespace_ref(&mut self, expr: ExprId) -> Option<Resolution> {
        match &self.store.exprs[expr] {
            Expr::Ident(name) => self.resolve_ident(expr, name),
            Expr::Path(path) => self
                .db
                .resolve_to_file(self.resolver.this().file(), path)
                .map(Resolution::Module),
            Expr::Member { obj, prop } => match self.namespace_ref(*obj)? {
                Resolution::Type(ty) => match ty.def_id(self.db) {
                    Some(def @ (DefId::Contract(_) | DefId::Interface(_) | DefId::Library(_))) => {
                        self.scope_member(&def, prop)
                    }
                    _ => None,
                },
                Resolution::Module(file) => self.file_member_ref(file, prop),
                _ => None,
            },
            _ => None,
        }
    }

    /// `name` in `def`'s own member scope — `Types` for namespace chains
    /// (`C.E`), `Variables` for constant leaves (`C.K`).
    fn scope_member(&self, def: &DefId, name: &Name) -> Option<Resolution> {
        let defmap = self.resolver.def_map(def);
        let scope = defmap.defs.get(def)?.child_scope?;
        self.scope_resolution(defmap.scopes[scope].by_name.get(name)?)
    }

    fn file_member_ref(&self, file: FileId, name: &Name) -> Option<Resolution> {
        self.resolver.file_scopes(file).find_map(|(defmap, scope)| {
            defmap.scopes[scope]
                .by_name
                .get(name)
                .and_then(|member| self.scope_resolution(member))
        })
    }

    /// The `Resolution` a scope member denotes — types (`C.E` path segments)
    /// and variables (`C.K` constant leaves); other namespaces are not
    /// foldable.
    fn scope_resolution(&self, member: &ScopeData) -> Option<Resolution> {
        match member.namespace {
            DefNamespace::Types => match Resolution::from_type(self.db, member.defs[0]) {
                resolution @ Resolution::Type(_) => Some(resolution),
                _ => None,
            },
            DefNamespace::Variables => Some(Resolution::Var(member.defs[0])),
            _ => None,
        }
    }

    /// A `constant`'s initializer evaluates in the variable's own store and
    /// scope. Failures inside it are `Foreign` — the constant's own check owns
    /// them — except a cycle re-entry, which every reference reports.
    fn eval_var(&mut self, expr: ExprId, var_def: DefId) -> Result<ConstValue, ConstError> {
        let DefId::Var(var_id) = var_def else {
            return Err(self.not_constant(expr));
        };
        let data = self.db.var_data(var_id);
        if data.kind != VariableKind::Const {
            return Err(self.not_constant(expr));
        }
        if self.visiting.contains(&var_def) {
            return Err(ConstError {
                range: self.range(expr),
                kind: ConstErrorKind::Cyclic(var_def),
            });
        }
        let Some(init) = data.init else {
            return Err(self.not_constant(expr));
        };
        let resolver = Resolver::for_declaration(self.db, var_def);
        let mut visiting = std::mem::take(&mut self.visiting);
        visiting.push(var_def);
        let mut child = ConstEval {
            db: self.db,
            store: &data.syntax_store,
            resolver: &resolver,
            source_map: None,
            visiting,
        };
        let value = child.eval(init);
        child.visiting.pop();
        self.visiting = child.visiting;
        value.map_err(|mut error| {
            error.range = self.range(expr);
            match error.kind {
                // A referenced non-constant makes this use non-constant too —
                // solc reports at both declarations. Evaluation failures are
                // the child's own: its const-init check owns them.
                ConstErrorKind::NotConstant | ConstErrorKind::Cyclic(_) => {}
                _ => error.kind = ConstErrorKind::Foreign,
            }
            error
        })
    }

    /// `T(x)` — evaluates `x` to a `ConstValue`, then applies the target
    /// type's wrap/pad semantics. Conversions the type level rejects still
    /// fail here — solc folds only legal conversions.
    fn convert(
        &self,
        expr: ExprId,
        ty: &Type,
        value: ConstValue,
    ) -> Result<ConstValue, ConstError> {
        // `uint256(keccak256(x))` is a legal constant of unknown value.
        if matches!(value, ConstValue::Opaque) {
            return Ok(ConstValue::Opaque);
        }
        match ty.peel_loc(self.db).kind(self.db) {
            TyKind::Primitive(primitive) => self.convert_primitive(expr, primitive, value),
            TyKind::Enum(id) => {
                let Some(value) = value.as_int() else {
                    return Err(self.not_constant(expr));
                };
                let variants = self.db.enum_data(id).variants.len();
                if value < BigInt::from(0u8) || value >= BigInt::from(variants) {
                    Err(self.eval_error(expr, "enum conversion out of range"))
                } else {
                    Ok(ConstValue::int(value))
                }
            }
            TyKind::Udvt(id) => {
                self.convert_primitive(expr, self.db.udvt_data(id).underlying, value)
            }
            _ => Err(self.not_constant(expr)),
        }
    }

    fn convert_primitive(
        &self,
        expr: ExprId,
        target: Primitive,
        value: ConstValue,
    ) -> Result<ConstValue, ConstError> {
        match target {
            Primitive::Uint(bits) => value
                .as_int()
                .map(|value| ConstValue::int(low_bits(&value, u32::from(bits))))
                .ok_or_else(|| self.eval_error(expr, "literal is not an integer")),
            Primitive::Int(bits) => {
                let Some(value) = value.as_int() else {
                    return Err(self.eval_error(expr, "literal is not an integer"));
                };
                let wrapped = low_bits(&value, u32::from(bits));
                let sign = BigInt::from(1u8) << (bits - 1);
                Ok(ConstValue::int(if wrapped >= sign {
                    wrapped - two_pow(u32::from(bits))
                } else {
                    wrapped
                }))
            }
            Primitive::Address | Primitive::AddressPayable => match value.as_int() {
                Some(value) => Ok(ConstValue::Bytes(to_be_bytes(&value, 20))),
                None => match value.as_bytes() {
                    Some(bytes) if bytes.len() == 20 => Ok(ConstValue::Bytes(bytes.into())),
                    _ => Err(self.not_constant(expr)),
                },
            },
            Primitive::Boolean => value
                .as_bool()
                .map(ConstValue::Bool)
                .ok_or_else(|| self.not_constant(expr)),
            Primitive::FixedBytes(width) => {
                let width = usize::from(width);
                if let Some(value) = value.as_int() {
                    return Ok(ConstValue::Bytes(to_be_bytes(&value, width)));
                }
                // `bytesN` keeps the leftmost bytes, right-padding short
                // input — `bytes2(hex"01ff")` is `0x01ff`, `bytes4(b"a")` is
                // `0x61000000`.
                value
                    .as_bytes()
                    .map(|bytes| {
                        let mut out = vec![0u8; width];
                        let keep = bytes.len().min(width);
                        out[..keep].copy_from_slice(&bytes[..keep]);
                        ConstValue::Bytes(out.into())
                    })
                    .ok_or_else(|| self.not_constant(expr))
            }
            Primitive::Bytes => value
                .as_bytes()
                .map(|bytes| ConstValue::Bytes(bytes.into()))
                .ok_or_else(|| self.not_constant(expr)),
            Primitive::String => match value.as_bytes().and_then(|bytes| String::from_utf8(bytes).ok()) {
                Some(text) => Ok(ConstValue::String(text.into())),
                None => Err(self.not_constant(expr)),
            },
            _ => Err(self.not_constant(expr)),
        }
    }
}
