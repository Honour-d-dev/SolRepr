use la_arena::ArenaMap;
use num_bigint::{BigInt, Sign};

use crate::hir::body_map::ScopeId;
use crate::hir::builtins::BuiltinFn;
use crate::hir::diagnostics::DiagnosticAnchor;
use crate::hir::exprs::{BinaryOp, Expr, ExprId, Literal, Name, UnaryOp};
use crate::hir::resolution::{Callable, CallableOrigin, Resolution};
use crate::hir::resolver::Resolver;
use crate::hir::syntax_store::SyntaxStore;
use crate::hir::types::VariableKind;
use crate::hir::types::{LiteralKind, Primitive, Ty, TyKind, Type};
use crate::ir::def_map::{DefId, TypeDefId};
use crate::salsa::hir_db::def_name;
use crate::salsa::interned_db::Var;
use crate::salsa::{FileId, HirDb};

use super::Inferer;

// ── Values ───────────────────────────────────────────────────────────────

/// Largest `**` exponent this evaluator builds: past it nothing fits 256 bits.
const POW_LIMIT: u32 = 4096;

const BIGINT_BITS: u64 = 1 << 20;

/// A folded compile-time value. Integer and rational arithmetic is exact;
/// an integer is a rational with `den == 1`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ConstValue {
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
    /// `type(C).*Code`.
    Opaque,
    /// A value too wide to build. Its literal kind is still exact, so a
    /// conversion rejects it on width without the value ever existing.
    Wide(LiteralKind),
}

impl ConstValue {
    fn int(value: impl Into<BigInt>) -> Self {
        Self::Rational(value.into(), BigInt::from(1u8))
    }

    /// No value is available — unknown, or too wide to build.
    fn is_opaque(&self) -> bool {
        matches!(self, Self::Opaque | Self::Wide(_))
    }

    /// The literal fitness this value denotes, when folding fixed it exactly.
    fn literal_kind(&self) -> Option<LiteralKind> {
        match self {
            Self::Rational(num, den) => {
                Some(LiteralKind::from_rational(num.clone(), den.clone(), None))
            }
            Self::Wide(kind) => Some(*kind),
            _ => None,
        }
    }

    /// The folded value as source-shaped text for hover. `None` when there is
    /// no value to show — an opaque constant, or one too wide to build.
    pub(crate) fn render(&self) -> Option<String> {
        match self {
            Self::Rational(num, den) if *den == BigInt::from(1u8) => Some(num.to_string()),
            Self::Rational(num, den) => Some(format!("{num}/{den}")),
            Self::Bool(value) => Some(value.to_string()),
            Self::Bytes(bytes) => Some(format!("hex\"{}\"", hex(bytes))),
            Self::String(text) => Some(quote(text)),
            Self::Opaque | Self::Wide(_) => None,
        }
    }

    pub(super) fn is_zero(&self) -> bool {
        matches!(self, Self::Rational(n, _) if *n == BigInt::from(0u8))
    }

    pub(super) fn rational(&self) -> Option<(BigInt, BigInt)> {
        match self {
            Self::Rational(num, den) => Some((num.clone(), den.clone())),
            _ => None,
        }
    }

    /// The exact integer value — `None` for non-integral or non-numeric values.
    pub(super) fn as_int(&self) -> Option<BigInt> {
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

// ConstValue Helper fns -------------------------------------------------------------
fn rat(num: BigInt, den: BigInt) -> ConstValue {
    if den < BigInt::from(0u8) {
        ConstValue::Rational(-num, -den)
    } else {
        ConstValue::Rational(num, den)
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// A Solidity string literal spelling `text`.
fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
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

/// The kind `num / den ** exponent` denotes past [`POW_LIMIT`]: only `0`, `1` and `-1` stay
/// representable, a fractional base or negative exponent stays fractional, and 257 bits says
/// "wider than any type" without building the value.
fn saturated_kind(num: &BigInt, den: &BigInt, exponent: i64) -> LiteralKind {
    let one = BigInt::from(1u8);
    if *den != one || exponent < 0 {
        return LiteralKind::Rational;
    }
    if *num == BigInt::from(0u8) {
        return LiteralKind::from_rational(BigInt::from(0u8), one, None);
    }
    let negative = num.sign() == Sign::Minus && exponent % 2 != 0;
    if num.magnitude() == &1u32.into() {
        let value = if negative { -one.clone() } else { one };
        return LiteralKind::from_rational(value, BigInt::from(1u8), None);
    }
    LiteralKind::Int {
        neg: negative,
        min_bits: 257,
        exact_bytes: None,
    }
}

/// The kind `value << shift` denotes past [`BIGINT_BITS`]: every nonzero value
/// is wider than any type, and zero was folded before this was needed.
fn saturated_shift_kind(value: &BigInt) -> LiteralKind {
    LiteralKind::Int {
        neg: value.sign() == Sign::Minus,
        min_bits: 257,
        exact_bytes: None,
    }
}

// ── Errors ───────────────────────────────────────────────────────────────

/// Why an expression does not fold to a [`ConstValue`]. `range` always
/// points into the *caller's* store: failures inside a foreign constant's
/// initializer re-anchor to the reference that reached it.
pub(super) struct ConstError {
    pub expr: ExprId,
    pub kind: ConstErrorKind,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ConstErrorKind {
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

/// A constant's folded value, keyed by its declaration. The value is a
/// property of the declaration, not of a body's inference run, so it is
/// memoized at the query. `ConstEval::eval_var` keeps inlining a referenced
/// constant's initializer, so the query never re-enters itself and a cycle
/// never becomes a salsa cycle.
#[salsa::tracked]
pub fn const_value<'db>(db: &'db dyn HirDb, v: Var<'db>) -> Result<ConstValue, ConstErrorKind> {
    let id = v.id(db);
    let data = db.var_data(id);
    if data.kind != VariableKind::Const {
        return Err(ConstErrorKind::NotConstant);
    }
    let Some(init) = data.init else {
        return Err(ConstErrorKind::NotConstant);
    };
    let def = DefId::Var(id);
    let resolver = Resolver::for_declaration(db, def);
    let mut eval = ConstEval::new(db, &data.syntax_store, &resolver, None, None);
    eval.visiting.push(def);
    eval.eval(init).map_err(|error| error.kind)
}

// ── Inferer entry points ─────────────────────────────────────────────────

impl<'db, 'store> Inferer<'db, 'store> {
    pub(super) fn eval_const(&self, expr: ExprId) -> Result<ConstValue, ConstError> {
        ConstEval::new(
            self.db,
            self.input.store,
            &self.resolver,
            self.input.expr_scopes,
            self.input.expr_orders,
        )
        .eval(expr)
    }

    /// `eval_const` for consumers needing the folded integer — array lengths.
    /// `None` means no positive integer; the diagnostic is reported here.
    pub(super) fn const_size(&mut self, expr: ExprId) -> Option<BigInt> {
        match self.eval_const(expr) {
            // solc keeps the length as `u256` — positive and < 2^256.
            Ok(value) => match value
                .as_int()
                .filter(|size| *size > BigInt::from(0u8) && *size < two_pow(256))
            {
                Some(size) => Some(size),
                // An opaque constant can't size the array — the type
                // lowers without one, but it is not an error.
                None if value.is_opaque() => None,
                None => self.fail(DiagnosticAnchor::Expr(expr), "invalid array length", None),
            },
            Err(error) => {
                self.report_const_error(error, "array size must be a constant expression");
                None
            }
        }
    }

    /// The literal kind a folded expression denotes; `None` when it cannot be folded. A `**`
    /// too wide to build still folds to an exact kind, so the conversion rejects it on width.
    pub(super) fn folded_literal(&self, expr: ExprId) -> Option<LiteralKind> {
        self.eval_const(expr).ok()?.literal_kind()
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
            ConstErrorKind::ZeroDivision { modulo } => if *modulo {
                "Modulo by zero."
            } else {
                "Division by zero."
            }
            .into(),
            ConstErrorKind::Eval(message) => message.clone(),
        };
        self.fail(DiagnosticAnchor::Expr(error.expr), message, ());
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
    expr_scopes: Option<&'a ArenaMap<ExprId, ScopeId>>,
    expr_orders: Option<&'a ArenaMap<ExprId, u32>>,
    visiting: Vec<DefId>,
}

impl<'a, 'db> ConstEval<'a, 'db> {
    pub(super) fn new(
        db: &'db dyn HirDb,
        store: &'a SyntaxStore,
        resolver: &'a Resolver<'db>,
        expr_scopes: Option<&'a ArenaMap<ExprId, ScopeId>>,
        expr_orders: Option<&'a ArenaMap<ExprId, u32>>,
    ) -> Self {
        Self {
            db,
            store,
            resolver,
            expr_scopes,
            expr_orders,
            visiting: Vec::new(),
        }
    }

    fn not_constant(&self, expr: ExprId) -> ConstError {
        ConstError {
            expr,
            kind: ConstErrorKind::NotConstant,
        }
    }

    fn eval_error(&self, expr: ExprId, message: impl Into<String>) -> ConstError {
        ConstError {
            expr,
            kind: ConstErrorKind::Eval(message.into()),
        }
    }
    fn resolve_ident(&self, expr: ExprId, name: &Name) -> Option<Resolution> {
        let scope = self.expr_scopes.and_then(|map| map.get(expr).copied());
        let order = self
            .expr_orders
            .and_then(|map| map.get(expr).copied())
            .unwrap_or_default();
        let is_self_name = self.store.name_expr == Some(expr);
        self.resolver
            .resolve_name_in(name, scope, order, is_self_name)
    }

    pub(super) fn eval(&mut self, expr: ExprId) -> Result<ConstValue, ConstError> {
        match self.store.exprs[expr].clone() {
            Expr::Literal(literal) => self.eval_literal(expr, &literal),
            Expr::Ident(name) => match self.resolve_ident(expr, &name) {
                Some(Resolution::Var(def)) => self.eval_var(expr, def),
                Some(Resolution::Variant(variant)) => {
                    Ok(ConstValue::int(u32::from(variant.id.into_raw())))
                }
                _ => Err(self.not_constant(expr)),
            },
            Expr::Binary { op, left, right } => self.eval_binary(expr, op, left, right),
            Expr::Unary { op, arg } => {
                let value = self.eval(arg)?;
                // An opaque operand yields an opaque result — still a legal
                // constant, just unevaluated.
                if value.is_opaque() && !matches!(op, UnaryOp::Delete) {
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
                    _ if cond.is_opaque() => {
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
                expr,
                kind: ConstErrorKind::ZeroDivision {
                    modulo: matches!(op, BinaryOp::Mod),
                },
            });
        }
        // An opaque operand yields an opaque result — still a legal
        // constant, just unevaluated.
        if left.is_opaque() || right.is_opaque() {
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
                let equal = self
                    .values_equal(&left, &right)
                    .ok_or_else(|| self.not_constant(expr))?;
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
                // Not built: `(2 ** 4096) ** 4096` is two megabytes, so the result's width is
                // capped first. Its kind stays exact, so the conversion rejects it on width.
                let too_wide =
                    |base: &BigInt| base.bits().saturating_mul(u64::from(unsigned)) > BIGINT_BITS;
                if unsigned > POW_LIMIT || too_wide(&num) || too_wide(&den) {
                    return Ok(ConstValue::Wide(saturated_kind(&num, &den, exponent)));
                }
                if exponent >= 0 {
                    Ok(rat(num.pow(unsigned), den.pow(unsigned)))
                } else if num == BigInt::from(0u8) {
                    Err(ConstError {
                        expr,
                        kind: ConstErrorKind::ZeroDivision { modulo: false },
                    })
                } else {
                    Ok(rat(den.pow(unsigned), num.pow(unsigned)))
                }
            }
            BinaryOp::Shl | BinaryOp::Shr => {
                let value = left.as_int().ok_or_else(|| self.not_constant(expr))?;
                let shift = right.as_int().ok_or_else(|| self.not_constant(expr))?;
                let shift = u32::try_from(&shift)
                    .map_err(|_| self.eval_error(expr, "invalid shift amount"))?;
                // `1 << 4_000_000_000` would allocate half a gigabyte; past the width cap a
                // nonzero value is left unbuilt but keeps an exact-enough kind, so the
                // conversion rejects it on width. Zero folds outright.
                if matches!(op, BinaryOp::Shl)
                    && value.bits().saturating_add(u64::from(shift)) > BIGINT_BITS
                {
                    return Ok(if value == BigInt::from(0u8) {
                        ConstValue::int(0u8)
                    } else {
                        ConstValue::Wide(saturated_shift_kind(&value))
                    });
                }
                Ok(ConstValue::int(match op {
                    BinaryOp::Shl => value << shift,
                    _ => value >> shift,
                }))
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
        match self.resolve_const_expr(obj) {
            Some(Resolution::Type(ty)) => self.type_member(expr, &ty, prop),
            Some(Resolution::Module(file)) => self.file_member(expr, file, prop),
            // `.length` is the only value member solc folds — and only on
            // `bytes`, never `string`.
            _ if prop == "length" => match self.eval(obj)? {
                ConstValue::Bytes(bytes) => Ok(ConstValue::int(bytes.len())),
                value if value.is_opaque() => Ok(ConstValue::Opaque),
                _ => Err(self.not_constant(expr)),
            },
            _ => Err(self.not_constant(expr)),
        }
    }

    /// `T(x)` — a conversion of a folded argument. A builtin call resolves to
    /// its [`Callable`]: UDVT conversions are the identity on the value,
    /// the other foldable builtins (`keccak256`, `addmod`, …) yield `Opaque`,
    /// and every other callable is not a constant.
    fn eval_call(
        &mut self,
        expr: ExprId,
        callee: ExprId,
        args: &[ExprId],
    ) -> Result<ConstValue, ConstError> {
        match self.resolve_const_expr(callee) {
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
            Some(Resolution::Callable(callable)) => {
                let mut values = Vec::with_capacity(args.len());
                for arg in args {
                    if matches!(self.store.exprs[*arg], Expr::NamedArgs(_)) {
                        return Err(self.not_constant(expr));
                    }
                    values.push(self.eval(*arg)?);
                }
                match callable.origin() {
                    CallableOrigin::Builtin(BuiltinFn::Wrap(_) | BuiltinFn::Unwrap(_)) => {
                        match values.as_slice() {
                            [value] => Ok(value.clone()),
                            _ => Err(self.not_constant(expr)),
                        }
                    }
                    CallableOrigin::Builtin(builtin) if builtin.is_const() => {
                        Ok(ConstValue::Opaque)
                    }
                    // `new bytes(n)` / `new string(n)`
                    CallableOrigin::Allocation(ty)
                        if values.len() == 1
                            && matches!(
                                ty.peel_loc(self.db).kind(self.db),
                                TyKind::Primitive(Primitive::Bytes | Primitive::String)
                            ) =>
                    {
                        Ok(ConstValue::Opaque)
                    }
                    _ => Err(self.not_constant(expr)),
                }
            }
            _ => Err(self.not_constant(expr)),
        }
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
        if value.is_opaque() || index.is_opaque() {
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
        let Some(Resolution::Type(ty)) = self.resolve_const_expr(inner) else {
            return Err(self.not_constant(expr));
        };
        let bits = |width: u16| -> BigInt { two_pow(u32::from(width)) };
        match (ty.kind(self.db), prop.as_str()) {
            (TyKind::Primitive(Primitive::Uint(_)), "min") => Ok(ConstValue::int(0u8)),
            (TyKind::Primitive(Primitive::Uint(n)), "max") => {
                Ok(ConstValue::int(bits(n) - BigInt::from(1u8)))
            }
            (TyKind::Primitive(Primitive::Int(n)), "min") => Ok(ConstValue::int(-bits(n - 1))),
            (TyKind::Primitive(Primitive::Int(n)), "max") => {
                Ok(ConstValue::int(bits(n - 1) - BigInt::from(1u8)))
            }
            (TyKind::TypeDef(TypeDefId::Enum(_)), "min") => Ok(ConstValue::int(0u8)),
            (TyKind::TypeDef(TypeDefId::Enum(id)), "max") => {
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
            (TyKind::TypeDef(TypeDefId::Interface(_)), "interfaceId") => Ok(ConstValue::Opaque),
            (TyKind::TypeDef(TypeDefId::Contract(_)), "creationCode" | "runtimeCode") => {
                Ok(ConstValue::Opaque)
            }
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
            TyKind::TypeDef(TypeDefId::Enum(id)) => {
                let data = self.db.enum_data(id);
                data.variants
                    .iter()
                    .position(|(_, variant)| variant.name == *prop)
                    .map(ConstValue::int)
                    .ok_or_else(|| self.not_constant(expr))
            }
            _ => match self.type_member_ref(ty, prop) {
                Some(Resolution::Var(def)) => self.eval_var(expr, def),
                _ => Err(self.not_constant(expr)),
            },
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
        match self
            .resolver
            .member_named(Ty::new(self.db, TyKind::Module(file)), prop)
            .effective_resolution()
        {
            Some(Resolution::Var(def)) => self.eval_var(expr, def),
            _ => Err(self.not_constant(expr)),
        }
    }

    /// `prop` on a type name, through the one member-lookup module.
    fn type_member_ref(&self, ty: &Type, prop: &Name) -> Option<Resolution> {
        self.resolver
            .member_named(Ty::new(self.db, TyKind::Type(*ty)), prop)
            .effective_resolution()
    }

    /// Resolves exprs that are valid in a const context otherwise `None`.
    //  It uses the resolver rather than the inference tables, because const-eval runs while
    /// types are lowered, before those tables exist.
    fn resolve_const_expr(&mut self, expr: ExprId) -> Option<Resolution> {
        match &self.store.exprs[expr] {
            Expr::Ident(name) => self.resolve_ident(expr, name),
            // `new bytes(n)`/`new string(n)` is the one constant `new` form: it denotes an allocation,
            // where contract creation is a call.
            Expr::New { ty } => match self.resolve_const_expr(*ty)? {
                Resolution::Type(name) => {
                    let name = name.peel_loc(self.db);
                    matches!(
                        name.kind(self.db),
                        TyKind::Primitive(Primitive::Bytes | Primitive::String)
                    )
                    .then(|| Resolution::Callable(Callable::new_allocation(name)))
                }
                _ => None,
            },
            Expr::Path(path) => self
                .db
                .resolve_to_file(self.resolver.this().file(), path)
                .map(Resolution::Module),
            // A member is answered by the member-lookup module given the receiver's type: declared-only,
            // builtin, or a builtin object's member.
            Expr::Member { obj, prop } => {
                let receiver = match self.resolve_const_expr(*obj)? {
                    Resolution::Type(ty) => Ty::new(self.db, TyKind::Type(ty)),
                    Resolution::Module(file) => Ty::new(self.db, TyKind::Module(file)),
                    Resolution::Builtin(object) => Ty::new(self.db, TyKind::Builtin(object)),
                    _ => return None,
                };
                self.resolver
                    .member_named(receiver, prop)
                    .effective_resolution()
            }
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
                expr,
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
            expr_scopes: None,
            expr_orders: None,
            visiting,
        };
        let value = child.eval(init);
        child.visiting.pop();
        self.visiting = child.visiting;
        value.map_err(|mut error| {
            error.expr = expr;
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
    /// fail here
    fn convert(
        &self,
        expr: ExprId,
        ty: &Type,
        value: ConstValue,
    ) -> Result<ConstValue, ConstError> {
        // `uint256(keccak256(x))` is a legal constant of unknown value.
        if value.is_opaque() {
            return Ok(ConstValue::Opaque);
        }
        match ty.peel_loc(self.db).kind(self.db) {
            TyKind::Primitive(primitive) => self.convert_primitive(expr, primitive, value),
            TyKind::TypeDef(TypeDefId::Enum(id)) => {
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
            TyKind::TypeDef(TypeDefId::Udvt(id)) => {
                self.convert_primitive(expr, self.db.udvt_data(id).underlying, value)
            }
            // A contract or interface value is an address, so `D(address(0))` is a constant of the
            // contract type; whether the conversion is legal is the call checker's question.
            TyKind::TypeDef(TypeDefId::Contract(_) | TypeDefId::Interface(_)) => {
                self.convert_primitive(expr, Primitive::Address, value)
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
            Primitive::String => match value
                .as_bytes()
                .and_then(|bytes| String::from_utf8(bytes).ok())
            {
                Some(text) => Ok(ConstValue::String(text.into())),
                None => Err(self.not_constant(expr)),
            },
            _ => Err(self.not_constant(expr)),
        }
    }
}
