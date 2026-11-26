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
    /// M bits in steps of 8 up to 256, N decimal places up to 80. Bare
    /// `fixed`/`ufixed` spell `fixed128x18`/`ufixed128x18`.
    Fixed(u16, u8),
    UFixed(u16, u8),
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
            Primitive::Fixed(m, n) => write!(f, "fixed{m}x{n}"),
            Primitive::UFixed(m, n) => write!(f, "ufixed{m}x{n}"),
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
            // `bytes(string)`, `string(bytes)`, and `bytesN(bytes)` — never the
            // reverse: nothing converts *to* `bytes` from a fixed-size array.
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
            // A sign change costs more than a widening: solc prefers `uint256` over `int256`, and reports
            // ambiguity only when the distances are equal.
            (Primitive::Uint(n), Primitive::Int(m)) if *m > *n => Some(2),
            (Primitive::FixedBytes(n), Primitive::FixedBytes(m)) if *m > *n => Some(1),
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

    /// Spelling-exact, not just width-exact: solc's keyword table holds canonical
    /// spellings only, so `uint08` is a legal user identifier, not a uint.
    /// Resolution shadows declarations with primitives, so leniency here would
    /// misresolve the names solc accepts.
    #[inline]
    pub fn parse(ty: &str) -> Primitive {
        match ty {
            "address" => return Primitive::Address,
            "address payable" => return Primitive::AddressPayable,
            // `payable(x)` is the conversion to `address payable.
            "payable" => return Primitive::AddressPayable,
            "bool" => return Primitive::Boolean,
            "bytes" => return Primitive::Bytes,
            "string" => return Primitive::String,
            "uint" => return Primitive::Uint(256),
            "int" => return Primitive::Int(256),
            "fixed" => return Primitive::Fixed(128, 18),
            "ufixed" => return Primitive::UFixed(128, 18),
            _ => {}
        }
       
        if let Some(size) = ty.strip_prefix("uint").and_then(canonical_width)
            && (8..=256).contains(&size)
            && size % 8 == 0
        {
            return Primitive::Uint(size);
        }
        if let Some(size) = ty.strip_prefix("int").and_then(canonical_width)
            && (8..=256).contains(&size)
            && size % 8 == 0
        {
            return Primitive::Int(size);
        }
        if let Some(size) = ty.strip_prefix("bytes").and_then(canonical_width)
            && (1..=32).contains(&size)
        {
            return Primitive::FixedBytes(size as u8);
        }
        if let Some((m, n)) = ty.strip_prefix("ufixed").and_then(split_widths)
            && m % 8 == 0
            && (8..=256).contains(&m)
            && n <= 80
        {
            return Primitive::UFixed(m, n as u8);
        }
        if let Some((m, n)) = ty.strip_prefix("fixed").and_then(split_widths)
            && m % 8 == 0
            && (8..=256).contains(&m)
            && n <= 80
        {
            return Primitive::Fixed(m, n as u8);
        }
        Primitive::Undefined //@TODO remove unknown and make this optional
    }
}

/// The width `digits` spells when the spelling is canonical — no canonical
/// width carries a leading zero, so any that does is a user identifier.
fn canonical_width(digits: &str) -> Option<u16> {
    if digits.len() > 1 && digits.starts_with('0') {
        return None;
    }
    digits.parse().ok()
}

/// The `MxN` halves of a fixed-point spelling, each canonical.
fn split_widths(rest: &str) -> Option<(u16, u16)> {
    let (m, n) = rest.split_once('x')?;
    Some((canonical_width(m)?, canonical_width(n)?))
}
