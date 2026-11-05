use crate::hir::types::*;
use crate::ir::def_map::DefId;

fn primitive(value: Primitive) -> TyKind {
    TyKind::Primitive(value)
}


#[test]
fn error_and_event_type_display_include_parameter_lists() {
    let (db, file) = crate::hir::tests::helper::make_test_db(
        r#"
        contract C {
            error Failure(uint256 code, address);
            event Changed(address indexed from, uint256 indexed tokenId);
        }
        "#,
    );
    let DefId::Error(error) = crate::hir::tests::helper::find_error(&db, file, "Failure") else {
        unreachable!()
    };
    let DefId::Event(event) = crate::hir::tests::helper::find_event(&db, file, "Changed") else {
        unreachable!()
    };

    assert_eq!(
        TyKind::Error(error).display(&db),
        "error Failure(uint256 code, address)"
    );
    assert_eq!(
        TyKind::Event(event).display(&db),
        "event Changed(address from, uint256 tokenId)"
    );
}

#[test]
fn primitive_implicit_conversions_match_solidity_directions() {
    assert_eq!(
        Primitive::Uint(8).implicitly_converts_to(&Primitive::Uint(16)),
        Some(1)
    );
    assert_eq!(
        Primitive::Uint(16).implicitly_converts_to(&Primitive::Uint(8)),
        None
    );
    assert_eq!(
        Primitive::Int(8).implicitly_converts_to(&Primitive::Int(16)),
        Some(1)
    );
    assert_eq!(
        Primitive::Uint(8).implicitly_converts_to(&Primitive::Int(16)),
        Some(1)
    );
    assert_eq!(
        Primitive::Int(8).implicitly_converts_to(&Primitive::Uint(16)),
        None
    );
    assert_eq!(
        Primitive::FixedBytes(4).implicitly_converts_to(&Primitive::FixedBytes(8)),
        Some(1)
    );
    assert_eq!(
        Primitive::AddressPayable.implicitly_converts_to(&Primitive::Address),
        Some(1)
    );
    assert_eq!(
        Primitive::Address.implicitly_converts_to(&Primitive::AddressPayable),
        None
    );
    assert_eq!(
        Primitive::Address.implicitly_converts_to(&Primitive::FixedBytes(20)),
        None
    );
    assert_eq!(
        Primitive::FixedBytes(20).implicitly_converts_to(&Primitive::Address),
        None
    );
    // solc: fixed-size byte arrays implicitly convert to `bytes`.
    assert_eq!(
        Primitive::FixedBytes(4).implicitly_converts_to(&Primitive::Bytes),
        Some(1)
    );
    assert_eq!(
        Primitive::String.implicitly_converts_to(&Primitive::Bytes),
        None
    );
    assert_eq!(
        Primitive::Bytes.implicitly_converts_to(&Primitive::String),
        None
    );
    assert_eq!(
        Primitive::parse("address payable"),
        Primitive::AddressPayable
    );
    assert_eq!(Primitive::parse("uint"), Primitive::Uint(256));
    assert_eq!(Primitive::parse("uint7"), Primitive::Undefined);
    assert_eq!(Primitive::parse("bytes33"), Primitive::Undefined);
}

#[test]
fn invalid_types_never_convert() {
    let (db, _) = crate::hir::tests::helper::make_test_db("contract C {}");
    let uint = Type(Ty::primitive(&db, Primitive::Uint(256)));
    let invalid = Type(Ty::invalid(&db));
    assert_eq!(invalid.implicitly_converts_to(uint, &db), None);
    assert_eq!(uint.implicitly_converts_to(invalid, &db), None);

    // The same holds through a `Located` — invalid never converts even when
    // the other side carries a data location.
    let bytes = Type::primitive(&db, Primitive::Bytes);
    let memory = bytes.at_loc(&db, Location::Memory);
    assert_eq!(invalid.implicitly_converts_to(memory, &db), None);
    assert_eq!(memory.implicitly_converts_to(invalid, &db), None);
}

/// A conversion *target* is a type someone wrote down — a declaration, a
/// parameter, an assignment's left-hand side — so it must be in the
/// nameable subset `N`. Non-assignable kinds must never be reachable as a
/// target, or `type(T)` and friends would silently participate in
/// assignment and argument checking.

#[test]
fn non_assignable_types_are_never_conversion_targets() {
    let (db, file) = crate::hir::tests::helper::make_test_db("contract C {}");
    let uint = Ty::primitive(&db, Primitive::Uint(256));
    let name = uint.narrow(&db).unwrap();
    let byte = Ty::primitive(&db, Primitive::Uint(8));

    let targets = [
        Ty::new(&db, TyKind::Meta(name)),
        Ty::new(&db, TyKind::Type(name)),
        Ty::new(
            &db,
            TyKind::Builtin(crate::hir::builtins::BuiltinTy::Msg),
        ),
        Ty::new(&db, TyKind::Void),
        Ty::new(&db, TyKind::Literal(LiteralKind::Rational)),
    ];

    for target in targets {
        let kind = target.kind(&db);
        assert!(
            !kind.is_nameable(),
            "{} should not be nameable",
            kind.display(&db)
        );
        assert_eq!(
            byte.implicitly_converts_to(target, &db),
            None,
            "converted to the non-nameable {}",
            kind.display(&db)
        );
        assert_eq!(
            target.implicitly_converts_to(target, &db),
            None,
            "the non-nameable {} converted to itself",
            kind.display(&db)
        );
    }

    // A tuple is the one non-nameable conversion target: element-wise, with
    // matching arity, which is what makes destructuring assignment work.
    let bytes = byte.narrow(&db).unwrap();
    let pair = Ty::tuple(&db, Box::new([name, name])).unwrap();
    assert_eq!(pair.implicitly_converts_to(pair, &db), Some(0));
    let widening = Ty::tuple(&db, Box::new([bytes, bytes])).unwrap();
    assert!(
        widening.implicitly_converts_to(pair, &db).is_some(),
        "a tuple converts element-wise"
    );
    assert_eq!(
        Ty::tuple(&db, Box::new([name, name, name]))
            .unwrap()
            .implicitly_converts_to(pair, &db),
        None,
        "arity must match"
    );
    let holed = Ty::tuple_from(&db, Box::new([Some(name.ty()), None]));
    assert_eq!(
        pair.implicitly_converts_to(holed, &db),
        Some(0),
        "a hole in the target accepts any source element"
    );

    // `super` and an imported module are receivers, not values. `super`
    // names a contract, but must not convert to it — a receiver is never
    // something you can assign.
    let DefId::Contract(contract) = crate::hir::tests::helper::find_contract(&db, file, "C") else {
        unreachable!("fixture declares contract C")
    };
    let contract_ty = Ty::new(&db, TyKind::Contract(contract));
    let super_ty = Ty::new(&db, TyKind::Super(contract));
    let module = Ty::new(&db, TyKind::Module(file));

    for source in [super_ty, module] {
        let kind = source.kind(&db);
        assert_eq!(
            source.implicitly_converts_to(contract_ty, &db),
            None,
            "the receiver {} converted to its contract",
            kind.display(&db)
        );
        assert_eq!(
            source.implicitly_converts_to(uint, &db),
            None,
            "the receiver {} converted to uint",
            kind.display(&db)
        );
    }
}

/// The data-location half of implicit conversion. A storage *pointer* (a
/// local or a storage parameter) only accepts a storage source; a bound
/// storage reference (a state variable, or a member/index) also accepts
/// `memory`/`calldata`, which are copied in. Nothing converts into
/// `calldata`.

#[test]
fn reference_kinds_decide_the_data_location_rule() {
    use crate::hir::types::Location::{Calldata, Memory};
    use RefKind::{Pointer, Reference};

    let bound = Location::Storage(Reference);
    let pointer = Location::Storage(Pointer);

    for from in [Memory, bound, Calldata] {
        assert!(Memory.accepts(from), "{from:?} -> memory");
    }

    assert!(bound.accepts(Memory));
    assert!(bound.accepts(Calldata));
    assert!(bound.accepts(bound));
    assert!(!pointer.accepts(Memory));
    assert!(!pointer.accepts(Calldata));
    assert!(pointer.accepts(bound));

    assert!(Calldata.accepts(Calldata));
    assert!(!Calldata.accepts(Memory));
    assert!(!Calldata.accepts(bound));
}
/// The explicit relation *defaults to* the implicit one and widens it, so the
/// primitive table is checked directly here; the end-to-end cast path is
/// `hir::inference::tests::conversion::explicit_conversions_match_solc`.
#[test]
fn primitive_explicit_conversions_match_solidity_directions() {
    // Integer to integer: same width, or same signedness.
    assert!(
        Primitive::Uint(256)
            .explicitly_converts_to(&Primitive::Uint(8))
            .is_some()
    );
    assert!(
        Primitive::Uint(8)
            .explicitly_converts_to(&Primitive::Int(8))
            .is_some()
    );
    assert!(
        Primitive::Int(8)
            .explicitly_converts_to(&Primitive::Uint(8))
            .is_some()
    );
    assert!(
        Primitive::Int(16)
            .explicitly_converts_to(&Primitive::Uint(8))
            .is_none()
    );
    // 160 bits, unsigned, non-payable only.
    assert!(
        Primitive::Uint(160)
            .explicitly_converts_to(&Primitive::Address)
            .is_some()
    );
    assert!(
        Primitive::Uint(256)
            .explicitly_converts_to(&Primitive::Address)
            .is_none()
    );
    assert!(
        Primitive::Address
            .explicitly_converts_to(&Primitive::Uint(160))
            .is_some()
    );
    assert!(
        Primitive::Address
            .explicitly_converts_to(&Primitive::FixedBytes(20))
            .is_some()
    );
    assert!(
        Primitive::AddressPayable
            .explicitly_converts_to(&Primitive::Uint(160))
            .is_none()
    );
    // Any address to any address, so `payable(address)` works.
    assert!(
        Primitive::Address
            .explicitly_converts_to(&Primitive::AddressPayable)
            .is_some()
    );
    // Fixed bytes: matching width to an *unsigned* integer, 20 bytes to an
    // address, and any fixed bytes to any other.
    assert!(
        Primitive::FixedBytes(32)
            .explicitly_converts_to(&Primitive::Uint(256))
            .is_some()
    );
    assert!(
        Primitive::FixedBytes(32)
            .explicitly_converts_to(&Primitive::Int(256))
            .is_none()
    );
    assert!(
        Primitive::FixedBytes(20)
            .explicitly_converts_to(&Primitive::Address)
            .is_some()
    );
    assert!(
        Primitive::FixedBytes(3)
            .explicitly_converts_to(&Primitive::Address)
            .is_none()
    );
    assert!(
        Primitive::FixedBytes(32)
            .explicitly_converts_to(&Primitive::FixedBytes(1))
            .is_some()
    );
    // `bytes`/`string` both ways, and `bytes` (not `string`) to `bytesN`.
    assert!(
        Primitive::Bytes
            .explicitly_converts_to(&Primitive::String)
            .is_some()
    );
    assert!(
        Primitive::String
            .explicitly_converts_to(&Primitive::Bytes)
            .is_some()
    );
    assert!(
        Primitive::Bytes
            .explicitly_converts_to(&Primitive::FixedBytes(32))
            .is_some()
    );
    assert!(
        Primitive::String
            .explicitly_converts_to(&Primitive::FixedBytes(32))
            .is_none()
    );
    // `bytesN` reaches `bytes` implicitly — so explicitly too.
    assert!(
        Primitive::FixedBytes(32)
            .explicitly_converts_to(&Primitive::Bytes)
            .is_some()
    );
    // A bool is only itself.
    assert!(
        Primitive::Boolean
            .explicitly_converts_to(&Primitive::Uint(8))
            .is_none()
    );
    assert!(
        Primitive::Uint(8)
            .explicitly_converts_to(&Primitive::Boolean)
            .is_none()
    );
}

/// solc rejects `uint8(-1)` and accepts `int8(-1)`. Pinned on the relation
/// because a negative literal cannot be written in a cast yet — unary minus is
/// not lowered, so `-1` arrives as `1` (see the unary entry in `TODO.md`).
#[test]
fn negative_literals_do_not_reach_unsigned_targets() {
    let (db, _) = crate::hir::tests::helper::make_test_db("contract C {}");
    let negative = LiteralKind::Int {
        neg: true,
        min_bits: 0,
        exact_bytes: None,
    };
    let unsigned = TyKind::Primitive(Primitive::Uint(8));
    let signed = TyKind::Primitive(Primitive::Int(8));

    assert!(negative.implicitly_converts_to(&unsigned).is_none());
    assert!(negative.explicitly_converts_to(&unsigned, &db).is_none());
    assert!(negative.explicitly_converts_to(&signed, &db).is_some());
}

#[test]
fn integer_literals_are_value_sensitive() {
    // min_bits: bits(v) for non-negative, bits(|v| - 1) for negative.
    let small = LiteralKind::Int {
        neg: false,
        min_bits: 8, // 255
        exact_bytes: None,
    };
    let large = LiteralKind::Int {
        neg: false,
        min_bits: 9, // 256
        exact_bytes: None,
    };
    let negative = LiteralKind::Int {
        neg: true,
        min_bits: 0, // -1
        exact_bytes: None,
    };

    assert!(
        small
            .implicitly_converts_to(&primitive(Primitive::Uint(8)))
            .is_some()
    );
    assert!(
        large
            .implicitly_converts_to(&primitive(Primitive::Uint(8)))
            .is_none()
    );
    assert!(
        negative
            .implicitly_converts_to(&primitive(Primitive::Int(8)))
            .is_some()
    );
    assert!(
        negative
            .implicitly_converts_to(&primitive(Primitive::Uint(8)))
            .is_none()
    );
}

#[test]
fn integer_literal_signed_boundaries() {
    // `min_bits < N` is the intN fitness test for both signs.
    let int = |neg, min_bits| LiteralKind::Int {
        neg,
        min_bits,
        exact_bytes: None,
    };
    assert!(
        int(false, 7)
            .implicitly_converts_to(&primitive(Primitive::Int(8)))
            .is_some()
    ); // 127
    assert!(
        int(false, 8)
            .implicitly_converts_to(&primitive(Primitive::Int(8)))
            .is_none()
    ); // 128
    assert!(
        int(false, 8)
            .implicitly_converts_to(&primitive(Primitive::Int(16)))
            .is_some()
    );
    assert!(
        int(true, 7)
            .implicitly_converts_to(&primitive(Primitive::Int(8)))
            .is_some()
    ); // -128
    assert!(
        int(true, 8)
            .implicitly_converts_to(&primitive(Primitive::Int(8)))
            .is_none()
    ); // -129
    assert!(
        int(true, 8)
            .implicitly_converts_to(&primitive(Primitive::Int(16)))
            .is_some()
    );
}

#[test]
fn hex_number_literals_have_exact_width_conversions() {
    let address_sized = LiteralKind::Int {
        neg: false,
        min_bits: 160,
        exact_bytes: Some(20),
    };
    assert!(
        address_sized
            .implicitly_converts_to(&primitive(Primitive::Address))
            .is_some()
    );
    assert!(
        address_sized
            .implicitly_converts_to(&primitive(Primitive::AddressPayable))
            .is_some()
    );
    assert!(
        address_sized
            .implicitly_converts_to(&primitive(Primitive::FixedBytes(20)))
            .is_some()
    );
    assert!(
        address_sized
            .implicitly_converts_to(&primitive(Primitive::FixedBytes(19)))
            .is_none()
    );
    // Still an integer literal: uintN conversion works too.
    assert!(
        address_sized
            .implicitly_converts_to(&primitive(Primitive::Uint(160)))
            .is_some()
    );
}

#[test]
fn rational_literals_only_convert_when_integral() {
    // Integral rationals fold to `Int` at parse time; the bare `Rational`
    // marker is non-integral and converts to nothing.
    let integral = LiteralKind::Int {
        neg: false,
        min_bits: 2,
        exact_bytes: None,
    };
    assert!(
        integral
            .implicitly_converts_to(&primitive(Primitive::Uint(8)))
            .is_some()
    );
    assert_eq!(
        LiteralKind::Rational.implicitly_converts_to(&primitive(Primitive::Uint(8))),
        None
    );
}

#[test]
fn string_literals_have_literal_only_byte_conversions() {
    let literal = LiteralKind::String { utf8: true, len: 3 };
    let hex_literal = LiteralKind::Hex { len: 2 };

    assert!(
        literal
            .implicitly_converts_to(&primitive(Primitive::String))
            .is_some()
    );
    assert!(
        literal
            .implicitly_converts_to(&primitive(Primitive::Bytes))
            .is_some()
    );
    assert!(
        literal
            .implicitly_converts_to(&primitive(Primitive::FixedBytes(3)))
            .is_some()
    );
    assert!(
        literal
            .implicitly_converts_to(&primitive(Primitive::FixedBytes(2)))
            .is_none()
    );
    assert!(
        hex_literal
            .implicitly_converts_to(&primitive(Primitive::FixedBytes(2)))
            .is_some()
    );
    assert!(
        hex_literal
            .implicitly_converts_to(&primitive(Primitive::FixedBytes(1)))
            .is_none()
    );
    assert_eq!(
        LiteralKind::String {
            utf8: false,
            len: 3
        }
        .implicitly_converts_to(&primitive(Primitive::String)),
        None
    );
}
