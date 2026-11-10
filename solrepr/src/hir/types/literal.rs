use crate::hir::types::kind::TyKind;
use crate::hir::types::primitive::Primitive;
use crate::hir::types::syntax::Location;
use crate::ir::def_map::TypeDefId;
use crate::salsa::HirDb;

#[derive(PartialEq, Eq, Clone, Copy, Hash)]
pub enum LiteralKind {
    Int {
        neg: bool,
        min_bits: u16,
        exact_bytes: Option<u8>,
    },
    Rational,
    String {
        utf8: bool,
        len: u32,
    },
    Hex {
        len: u32,
    },
}

impl LiteralKind {
    /// The smallest Solidity integer bit-width.
    fn int_width(bits: u16) -> Option<u16> {
        let width = bits.max(1).div_ceil(8) * 8;
        (width <= 256).then_some(width)
    }

    pub fn implicitly_converts_to(&self, target: &TyKind) -> Option<u8> {
        match self {
            Self::Int {
                neg,
                min_bits,
                exact_bytes,
            } => match target {
                TyKind::Primitive(Primitive::Uint(bits)) if !neg && min_bits <= bits => Some(1),
                TyKind::Primitive(Primitive::Int(bits)) if min_bits < bits => Some(1),
                // Zero needs no bits, and `0` is the one literal every
                // `bytesNN` accepts; nonzero values convert only at their
                // exact byte width.
                TyKind::Primitive(Primitive::FixedBytes(bits))
                    if *exact_bytes == Some(*bits) || *min_bits == 0 =>
                {
                    Some(1)
                }
                TyKind::Primitive(Primitive::Address | Primitive::AddressPayable)
                    if *exact_bytes == Some(20) =>
                {
                    Some(1)
                }
                _ => None,
            },
            Self::Rational => None,
            Self::String { utf8, len } => match target {
                TyKind::Primitive(Primitive::String) if *utf8 => Some(0),
                TyKind::Primitive(Primitive::Bytes) => Some(1),
                TyKind::Primitive(Primitive::FixedBytes(bits)) if *len as u64 <= *bits as u64 => {
                    Some(1)
                }
                _ => None,
            },
            Self::Hex { len } => match target {
                TyKind::Primitive(Primitive::Bytes) => Some(1),
                TyKind::Primitive(Primitive::FixedBytes(bits)) if *len as u64 <= *bits as u64 => {
                    Some(1)
                }
                _ => None,
            },
        }
    }

    pub fn explicitly_converts_to(&self, target: &TyKind, db: &dyn HirDb) -> Option<u8> {
        self.implicitly_converts_to(target)
            .or_else(|| self.explicit_widening(target, db))
    }

    pub(crate) fn explicit_widening(&self, target: &TyKind, db: &dyn HirDb) -> Option<u8> {
        let Self::Int { neg, min_bits, .. } = self else {
            return None;
        };
        let zero = *min_bits == 0;
        match target {
            TyKind::Primitive(Primitive::AddressPayable) if zero => Some(1),
            TyKind::Primitive(Primitive::Address)
                if zero
                    || (!neg && Self::int_width(*min_bits).is_some_and(|width| width <= 160)) =>
            {
                Some(1)
            }
            TyKind::Primitive(Primitive::FixedBytes(_)) if zero => Some(1),
            
            TyKind::TypeDef(TypeDefId::Enum(id)) if !neg => {
                let members = db.enum_data(*id).variants.len() as u128;
                let lower = if zero {
                    0
                } else {
                    1u128 << (*min_bits - 1).min(127)
                };
                (lower < members).then_some(1)
            }
            _ => None,
        }
    }

    pub fn inferred_type(&self) -> Option<TyKind> {
        match self {
            Self::Int { neg, min_bits, .. } => {
                let primitive = if *neg {
                    Self::int_width(min_bits.saturating_add(1)).map(Primitive::Int)
                } else {
                    Self::int_width(*min_bits).map(Primitive::Uint)
                };
                primitive.map(TyKind::Primitive)
            }
            Self::Rational => None,
            Self::String { .. } => Some(TyKind::Primitive(Primitive::String)),
            Self::Hex { len } => Some(TyKind::Primitive(if (1..=32).contains(len) {
                Primitive::FixedBytes(*len as u8)
            } else {
                Primitive::Bytes
            })),
        }
    }

    pub(crate) fn display(&self) -> String {
        match self {
            Self::Int { .. } => "int_const".into(),
            Self::Rational => "rational_const".into(),
            Self::String { .. } => "literal_string".into(),
            Self::Hex { .. } => "literal_hex".into(),
        }
    }

    pub(crate) fn default_loc(&self) -> Location {
        match self {
            Self::String { .. } | Self::Hex { .. } => Location::Memory,
            _ => Location::Stack,
        }
    }
}
