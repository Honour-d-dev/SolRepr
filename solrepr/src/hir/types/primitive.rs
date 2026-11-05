use std::fmt;

use crate::hir::types::syntax::Location;

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub enum Primitive {
    Address,
    AddressPayable,
    Boolean,
    Bytes,
    String,
    Uint(u16),
    Int(u16),
    FixedBytes(u8),
    Undefined,
}

impl fmt::Display for Primitive {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Primitive::Address => write!(f, "address"),
            Primitive::AddressPayable => write!(f, "address payable"),
            Primitive::Boolean => write!(f, "bool"),
            Primitive::Bytes => write!(f, "bytes"),
            Primitive::String => write!(f, "string"),
            Primitive::Uint(n) => write!(f, "uint{n}"),
            Primitive::Int(n) => write!(f, "int{n}"),
            Primitive::FixedBytes(n) => write!(f, "bytes{n}"),
            Primitive::Undefined => write!(f, "undefined"),
        }
    }
}

impl Primitive {
    pub fn explicitly_converts_to(&self, target: &Primitive) -> Option<u8> {
        self.implicitly_converts_to(target)
            .or_else(|| self.explicit_widening(target))
    }

    pub(crate) fn explicit_widening(&self, target: &Primitive) -> Option<u8> {
        use Primitive::*;
        match (self, target) {
            // Any address converts to any address, so `address(x)` and
            // `payable(x)` both work; the implicit rule is one-way.
            (Address | AddressPayable, Address | AddressPayable) => Some(1),
            // Integer to integer: same width, or same signedness.
            (Uint(_), Uint(_)) | (Int(_), Int(_)) => Some(1),
            (Uint(n), Int(m)) | (Int(n), Uint(m)) if n == m => Some(1),
            // Integer to `uint160`/`bytesN` — unsigned only.
            (Uint(160), Address) => Some(1),
            (Uint(n), FixedBytes(m)) if *n == *m as u16 * 8 => Some(1),
            // A *non-payable* address to `uint160`/`bytes20`; `address payable`
            // is excluded, exactly as solc's `m_stateMutability == NonPayable`
            // check does.
            (Address, Uint(160) | FixedBytes(20)) => Some(1),
            // Fixed bytes to an unsigned integer of the same width, or to a
            // non-payable address at 20 bytes, and to any fixed bytes — the
            // implicit rule only widens.
            (FixedBytes(m), Uint(n)) if *n == *m as u16 * 8 => Some(1),
            (FixedBytes(20), Address) => Some(1),
            (FixedBytes(_), FixedBytes(_)) => Some(1),
            // A `bytes`/`string` pair converts in both directions, and `bytes`
            // reaches any `bytesN` (solc checks the source kind, not the
            // length).
            (Bytes, String | FixedBytes(_)) | (String, Bytes) => Some(1),
            _ => None,
        }
    }

    /// Returns the cost of implicitly converting `self` to `target`.
    /// `None` means no implicit conversion is possible.
    pub fn implicitly_converts_to(&self, target: &Primitive) -> Option<u8> {
        match (self, target) {
            (a, b) if a == b => Some(0),
            (Primitive::Uint(n), Primitive::Uint(m)) if *m > *n => Some(1),
            (Primitive::Int(n), Primitive::Int(m)) if *m > *n => Some(1),
            (Primitive::Uint(n), Primitive::Int(m)) if *m > *n => Some(1),
            (Primitive::FixedBytes(n), Primitive::FixedBytes(m)) if *m > *n => Some(1),
            // solc: fixed-size byte arrays implicitly convert to `bytes`.
            (Primitive::FixedBytes(_), Primitive::Bytes) => Some(1),
            (Primitive::AddressPayable, Primitive::Address) => Some(1),
            _ => None,
        }
    }

    #[inline]
    pub fn default_loc(&self) -> Option<Location> {
        match self {
            Primitive::Bytes | Primitive::String => Some(Location::Memory),
            _ => None,
        }
    }

    /// All primitive types commonly used in Solidity, for completion.
    /// `Uint`/`Int` cover 8–256 in steps of 8; `FixedBytes` covers 1–32.
    pub fn all_primitives() -> Vec<Primitive> {
        let mut acc = vec![
            Primitive::Boolean,
            Primitive::Address,
            Primitive::AddressPayable,
            Primitive::String,
            Primitive::Bytes,
        ];
        // uint8 .. uint256
        for size in (8..=256).step_by(8) {
            acc.push(Primitive::Uint(size));
        }
        // int8 .. int256
        for size in (8..=256).step_by(8) {
            acc.push(Primitive::Int(size));
        }
        // bytes1 .. bytes32
        for size in 1..=32u8 {
            acc.push(Primitive::FixedBytes(size));
        }
        acc
    }

    #[inline]
    pub fn parse(ty: &str) -> Primitive {
        match ty {
            "address" => Primitive::Address,
            "address payable" => Primitive::AddressPayable,
            // `payable(x)` is the conversion to `address payable`; the keyword
            // is anonymous in the grammar, so the callee is synthesized as this
            // name and the call flows through the ordinary cast path.
            "payable" => Primitive::AddressPayable,
            "bool" => Primitive::Boolean,
            "bytes" => Primitive::Bytes,
            "string" => Primitive::String,
            "uint" => Primitive::Uint(256),
            "int" => Primitive::Int(256),
            s if s
                .strip_prefix("uint")
                .and_then(|size| size.parse::<u16>().ok())
                .is_some_and(|size| (8..=256).contains(&size) && size % 8 == 0) =>
            {
                Primitive::Uint(s[4..].parse().unwrap())
            }
            s if s
                .strip_prefix("int")
                .and_then(|size| size.parse::<u16>().ok())
                .is_some_and(|size| (8..=256).contains(&size) && size % 8 == 0) =>
            {
                Primitive::Int(s[3..].parse().unwrap())
            }
            s if s
                .strip_prefix("bytes")
                .and_then(|size| size.parse::<u8>().ok())
                .is_some_and(|size| (1..=32).contains(&size)) =>
            {
                Primitive::FixedBytes(s[5..].parse().unwrap())
            }
            _ => Primitive::Undefined, //@TODO remove unknown and make this optional
        }
    }
}
