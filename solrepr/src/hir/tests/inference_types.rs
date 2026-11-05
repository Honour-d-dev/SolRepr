use super::*;
use crate::ast::NodeRange;
use crate::hir::body_map::BodyOwnerId;
use crate::salsa::HirDb;
use num_bigint::BigInt;

#[test]
fn inherited_constant_in_signature_type_is_resolved() {
    let source = r#"
        contract Base {
            uint internal constant N = 4;
        }
        abstract contract Derived is Base {
            function f(uint[N] memory values) external;
        }
    "#;
    let (db, file) = make_test_db(source);
    let DefId::Function(function) = find_function(&db, file, "f") else {
        unreachable!()
    };

    let signature = db
        .function_signature(function)
        .expect("inherited array bound should resolve without body inference");
    assert_eq!(signature.params.len(), 1);
    let TyKind::Ref(array, crate::hir::types::Location::Memory) = signature.params[0].kind(&db)
    else {
        panic!("expected a memory reference parameter");
    };
    let TyKind::Array { elem, size } = array.kind(&db) else {
        panic!("expected a fixed-size array parameter");
    };
    assert_eq!(size, Some(BigInt::from(4u8)));
    assert!(matches!(
        elem.kind(&db),
        TyKind::Primitive(crate::hir::types::Primitive::Uint(256))
    ));
}

/// The size expression in `uint256[N]` is an ordinary value expression:
/// `infer_all`'s sweep types it, hover resolves it to the constant, and in
/// expression position (`abi.decode`'s type tuple — the only path reaching
/// `Expr::ArrayAccess`'s type arm with a size) it lowers to a fixed-size
/// array type.
#[test]
fn array_size_expressions_infer_and_type_as_fixed_arrays() {
    for source in [
        r#"
            contract C {
                uint256 constant N = 4;
                function f() external { uint256[N] memory a; }
            }
            "#,
        r#"
            contract C {
                uint256 constant N = 4;
                function f(bytes calldata data) external pure {
                    abi.decode(data, (uint256[N]));
                }
            }
            "#,
    ] {
        let (db, file) = make_test_db(source);
        let def = find_function(&db, file, "f");

        let offset = source
            .find("uint256[N]")
            .expect("the source should contain the array type")
            + "uint256[".len();
        let range = NodeRange {
            start: offset as u32,
            end: offset as u32 + 1,
        };

        let id = semantic_at(&db, def, range)
            .unwrap_or_else(|| panic!("the array size should map to a semantic id"));
        let SemanticId::Expr(expr) = id else {
            panic!("the array size should be an expression");
        };

        let inference = db.inference(def);
        assert!(
            !matches!(inference.expr_ty(expr).kind(&db), TyKind::Invalid),
            "the array size should be typed, not left as recovery",
        );
        let resolution = inference
            .expr_resolution(expr)
            .expect("the array size should carry a resolution for hover");
        assert!(
            matches!(resolution, Resolution::Var(_)),
            "the array size should resolve to the constant",
        );
    }

    let (db, file) = make_test_db(
        r#"
            contract C {
                uint256 constant N = 4;
                function f(bytes calldata data) external pure {
                    abi.decode(data, (uint256[N]));
                }
            }
            "#,
    );
    let inference = function_inference(&db, file);
    assert!(
        inference.diagnostics.is_empty(),
        "{:?}",
        codes(&inference),
    );
    let fixed = expr_results(&inference).any(|(_, ty, _)| {
        matches!(ty.kind(&db), TyKind::Type(inner)
            if matches!(inner.peel_loc(&db).kind(&db), TyKind::Array { size: Some(n), .. } if n == BigInt::from(4u8)))
    });
    assert!(
        fixed,
        "expected uint256[N] to type as a fixed-size array of 4"
    );
}

#[test]
fn invalid_data_location_reports_diagnostic() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function f() external { mapping(address => uint) memory m; }
            }
            "#,
    );
    let inference = function_inference(&db, file);
    let diagnostic = inference
        .diagnostics
        .iter()
        .find(|diagnostic| matches!(diagnostic, SemanticDiagnostic::InvalidDataLocation { .. }));
    let Some(SemanticDiagnostic::InvalidDataLocation { ty_ref_id, .. }) = diagnostic else {
        panic!("expected an invalid-data-location diagnostic");
    };
    let DefId::Function(id) = find_function(&db, file, "f") else {
        panic!("fixture function should exist");
    };
    let range = db
        .body_map(BodyOwnerId::Function(id))
        .syntax_store
        .ty_ref_ranges[*ty_ref_id];
    let text = db.text(file);
    assert!(text[range.start as usize..range.end as usize].contains("mapping"));
}

/// `Outer.Inner` lowers to a two-node `UserDefined` chain — the final node is
/// the last identifier, the base node is `Outer` — and each node keeps its
/// own identifier's range (not the whole path's), which is what makes member
/// hover work on each segment.
#[test]
fn qualified_types_lower_to_identifier_chains() {
    let (db, file) = make_test_db(
        r#"
            contract Outer {
                struct Inner { uint x; }
                function f() external { Outer.Inner value; }
            }
            "#,
    );
    let owner = find_function(&db, file, "f");
    let DefId::Function(function_id) = owner else {
        panic!("expected function owner");
    };
    let (body, _) = db.body_and_source_map(BodyOwnerId::Function(function_id));
    let local_id = body.scopes[body.root_scope]
        .get(&"value".into())
        .expect("fixture should declare `value`");
    let ty_ref_id = *body.local(*local_id).ty_ref();

    let TyName::UserDefined { base, name } = &body.syntax_store.types[ty_ref_id] else {
        panic!("expected `value` to have a user-defined type");
    };
    assert_eq!(name.as_str(), "Inner");
    let base_id = base.expect("Inner should have a base chain node");
    let TyName::UserDefined { base: root, name } = &body.syntax_store.types[base_id] else {
        panic!("expected base chain node to be user-defined");
    };
    assert!(root.is_none());
    assert_eq!(name.as_str(), "Outer");

    let text = db.text(file);
    let inner_range = body.syntax_store.ty_ref_ranges[ty_ref_id];
    let outer_range = body.syntax_store.ty_ref_ranges[base_id];
    assert_eq!(
        &text[inner_range.start as usize..inner_range.end as usize],
        "Inner"
    );
    assert_eq!(
        &text[outer_range.start as usize..outer_range.end as usize],
        "Outer"
    );
}

// ── Constant evaluation ─────────────────────────────────────────────────

/// The size expression in `uint256[N]` is a constant: a `constant` variable in
/// the var's own store or a signature's, a composed initializer, `**`'s
/// checked arithmetic, a member path through the type's own scope (`D.N`),
/// an enum member (`E.C`), a `bytes` constant's `.length`, a meta member
/// (`type(uint8).max`), operators, a ternary, a conversion, and exact rational
/// arithmetic (`1/2*4` is `2`, not `0`) all fold.
#[test]
fn array_sizes_fold_constant_expressions() {
    let fixtures: &[(&str, usize)] = &[
        (
            r#"
            contract C {
                uint256 constant N = 4;
                function f() external { uint256[N] memory a; }
            }
            "#,
            4,
        ),
        (
            r#"
            contract C {
                uint256 constant N = 4;
                uint256 constant M = N * 2;
                function f() external { uint256[M] memory a; }
            }
            "#,
            8,
        ),
        (
            "contract C { function f() external { uint256[2**8] memory a; } }",
            256,
        ),
        (
            // Type-name member access follows internal scoping — `D.N` only
            // reaches `C` when `C` is `D` or derived from it.
            r#"
            contract D { uint constant N = 5; }
            contract C is D {
                function f() external { uint[D.N] memory a; }
            }
            "#,
            5,
        ),
        (
            r#"
            enum E { A, B, C }
            contract C {
                function f() external { uint[E.C] memory a; }
            }
            "#,
            2,
        ),
        (
            r#"
            contract C {
                bytes constant B = hex"0102";
                function f() external { uint[B.length] memory a; }
            }
            "#,
            2,
        ),
    ];
    for (type_text, expected) in [
        ("uint[type(uint8).max]", 255usize),
        ("uint[-(-4)]", 4),
        ("uint[~(-5)]", 4),
        ("uint[true ? 3 : 9]", 3),
        ("uint[uint8(4)]", 4),
        ("uint[1/2*4]", 2),
        ("uint[1 << 4]", 16),
    ]
    .into_iter()
    .map(|(type_text, expected)| {
        (
            format!("contract C {{ function f() external {{ {type_text} memory a; }} }}"),
            expected,
        )
    })
    .chain(fixtures.iter().map(|(s, e)| (s.to_string(), *e)))
    {
        let (db, file) = make_test_db(&type_text);
        let inference = function_inference(&db, file);
        assert!(
            inference.diagnostics.is_empty(),
            "{type_text}: {:?}",
            codes(&inference)
        );
        let fixed = inference
            .types
            .iter()
            .any(|(_, ty)| matches!(ty.peel_loc(&db).kind(&db), TyKind::Array { size, .. } if size == Some(BigInt::from(expected))));
        assert!(fixed, "expected a fixed-size array of {expected}: {type_text}");
    }
}

/// Non-constant sizes report: a cyclic pair of constants, a parameter, an
/// `immutable`, and rational/negative/zero lengths — each with the diagnostic
/// that fits ("constant" for non-constants, "array length" for bad values).
#[test]
fn invalid_array_lengths_are_reported() {
    for source in [
        "contract C { uint256 constant A = B; uint256 constant B = A; function f() external { uint256[A] memory x; } }",
        "contract C { function f(uint256 n) external { uint256[n] memory x; } }",
        "contract C { uint immutable I = 4; function f() external { uint[I] memory a; } }",
    ] {
        let (db, file) = make_test_db(source);
        let inference = function_inference(&db, file);
        assert!(
            inference.diagnostics.iter().any(|diagnostic| {
                matches!(diagnostic, SemanticDiagnostic::InferenceFailure { message, .. }
                    if message.contains("constant"))
            }),
            "{source}: expected a non-constant diagnostic"
        );
    }

    for type_text in ["uint[-1]", "uint[1/2]", "uint[0]", "uint[type(int8).min]"] {
        let source = format!(
            "contract C {{ function f() external {{ {type_text} memory a; }} }}"
        );
        let (db, file) = make_test_db(&source);
        let inference = function_inference(&db, file);
        assert!(
            inference.diagnostics.iter().any(|diagnostic| {
                matches!(diagnostic, SemanticDiagnostic::InferenceFailure { message, .. }
                    if message.contains("array length"))
            }),
            "{type_text}: expected an invalid-array-length diagnostic"
        );
    }
}

/// `x / 0` is a compile error wherever the divisor folds — not only inside
/// constant contexts.
#[test]
fn constant_zero_divisor_is_an_error_anywhere() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function f() external pure { uint x = 1 / 0; }
            }
            "#,
    );
    let inference = function_inference(&db, file);
    assert!(inference.diagnostics.iter().any(|diagnostic| {
        matches!(diagnostic, SemanticDiagnostic::InferenceFailure { message, .. }
            if message.contains("Division by zero"))
    }));

    let (db, file) = make_test_db(
        r#"
            contract C {
                function f() external pure { uint x = 5 % 0; }
            }
            "#,
    );
    let inference = function_inference(&db, file);
    assert!(inference.diagnostics.iter().any(|diagnostic| {
        matches!(diagnostic, SemanticDiagnostic::InferenceFailure { message, .. }
            if message.contains("Modulo by zero"))
    }));
}

/// Variable declarations are checked: a `constant` initializer must fold,
/// `uint constant A;` must be initialized, a foreign failure reports at the
/// failing constant's own site (not the reference), a bad initializer is a
/// conversion error like a local's, and a self-read (`uint a = a`, `uint[a] a`)
/// is rejected — a declaration site resolves to itself for hover, but that is
/// the only legal self-resolution.
#[test]
fn variable_declarations_are_checked() {
    for source in [
        "contract C { uint constant A = f(); function f() internal pure returns (uint) { return 1; } }",
        "contract C { uint constant A; }",
        "contract C { uint constant B = g(); uint constant A = B; function g() internal pure returns (uint) { return 1; } }",
    ] {
        let (db, file) = make_test_db(source);
        let inference = var_inference(&db, file, "A");
        assert!(
            inference.diagnostics.iter().any(|diagnostic| {
                matches!(diagnostic, SemanticDiagnostic::InferenceFailure { message, .. }
                    if message.contains("constant"))
            }),
            "{source}: expected a constant diagnostic"
        );
    }

    let (db, file) = make_test_db("contract C { uint x = \"abc\"; }");
    let inference = var_inference(&db, file, "x");
    assert!(inference.diagnostics.iter().any(|diagnostic| {
        matches!(diagnostic, SemanticDiagnostic::TypeMismatch { .. })
    }));

    for source in ["contract C { uint a = a; }", "contract C { uint[a] a; }"] {
        let (db, file) = make_test_db(source);
        let inference = var_inference(&db, file, "a");
        assert!(
            inference
                .diagnostics
                .iter()
                .any(|diagnostic| matches!(diagnostic, SemanticDiagnostic::InferenceFailure { .. })),
            "{source}: expected a self-reference diagnostic"
        );
    }
}

/// Constructs solc folds but the evaluator does not implement —
/// `keccak256`, `addmod`, `type(I).interfaceId` — yield `ConstValue::Opaque`:
/// the initializer is a legal constant, so no diagnostic may appear. The
/// declaration still lowers and the call still infers.
#[test]
fn opaque_constants_stay_silent() {
    let (db, file) = make_test_db(
        r#"
            interface I { function f() external; }
            contract C {
                bytes32 constant SLOT = keccak256("eip1967.proxy.implementation");
                uint constant M = addmod(1, 2, 3);
                bytes4 constant IID = type(I).interfaceId;
            }
            "#,
    );
    for name in ["SLOT", "M", "IID"] {
        let inference = var_inference(&db, file, name);
        assert!(
            inference.diagnostics.is_empty(),
            "{name}: {:?}",
            inference
                .diagnostics
                .iter()
                .map(SemanticDiagnostic::code)
                .collect::<Vec<_>>()
        );
    }
    let inference = var_inference(&db, file, "SLOT");
    assert!(expr_results(&inference).any(|(_, ty, _)| {
        matches!(ty.kind(&db), TyKind::Primitive(crate::hir::types::Primitive::FixedBytes(32)))
    }), "the keccak256 call should still infer to bytes32");
}

#[test]
fn type_name_resolutions_are_never_modules_or_tuples() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function f(bytes memory data) external {
                    abi.decode(data, (uint256, address));
                }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "f");

    // The type list is a tuple whose components are `TyKind::Type` — the
    // *component type* says "this is a type name", so no resolution is involved
    // and the tuple carries no separate kind.
    assert!(
        expr_results(&inference).any(|(_, ty, _)| {
            matches!(ty.kind(&db), TyKind::Tuple(tuple)
            if tuple.elements.len() == 2
                && tuple.elements.iter().all(|elem| matches!(
                    elem.map(|ty| ty.kind(&db)),
                    Some(TyKind::Type(_))
                )))
        }),
        "expected the decode type list to type as a tuple of types"
    );
    // Named elements are plain `Type` resolutions.
    assert!(
        expr_results(&inference)
            .any(|(_, _, resolution)| matches!(resolution, Some(Resolution::Type(_)))),
        "expected the tuple's type names to resolve as `Type`"
    );
    // `Type` is the nameable subset only: modules and tuples have variants
    // of their own, so no consumer has to narrow a general `Ty` back down.
    for (_, _, resolution) in expr_results(&inference) {
        if let Some(Resolution::Type(ty)) = resolution {
            assert!(
                !matches!(ty.kind(&db), TyKind::Module(_) | TyKind::Tuple(_)),
                "`Resolution::Type` must only hold nameable kinds"
            );
        }
    }
}

/// A qualified type path reports once, on the failing segment; the rest of the
/// chain propagates recovery without cascading unresolved-type diagnostics.
#[test]
fn unresolved_type_segments_report_once_on_the_failing_segment() {
    let cases = [
        (
            r#"
            contract C {
                function f() external { Missing.Typed x; }
            }
            "#,
            "Missing",
        ),
        (
            r#"
            contract Outer {
                struct Inner { uint x; }
                function f(Outer.Missing.Inner value) external {}
            }
            "#,
            "Missing",
        ),
        (
            r#"
            contract Outer {
                struct Inner { uint x; }
                function f() external { Outer.Missing value; }
            }
            "#,
            "Missing",
        ),
    ];
    for (source, name) in cases {
        let (db, file) = make_test_db(source);
        let inference = function_inference(&db, file);
        let missing = inference
            .diagnostics
            .iter()
            .filter_map(|d| match d {
                SemanticDiagnostic::UnresolvedType { name, ty_ref_id, .. } => {
                    Some((name.as_str(), *ty_ref_id))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            missing.len(),
            1,
            "{source}: expected exactly one unresolved-type diagnostic"
        );
        let (found, ty_ref_id) = missing[0];
        assert_eq!(found, name, "{source}");
        let DefId::Function(id) = find_function(&db, file, "f") else {
            panic!("{source}: fixture function should exist");
        };
        let range = db
            .body_map(BodyOwnerId::Function(id))
            .syntax_store
            .ty_ref_ranges[ty_ref_id];
        let text = db.text(file);
        assert_eq!(
            &text[range.start as usize..range.end as usize],
            name,
            "{source}"
        );
    }
}

/// `internal` members of a library inline into every caller — a library
/// constant is visible to any contract through the type name. `private`
/// members stay invisible.
#[test]
fn library_internal_members_reach_any_caller() {
    let (db, file) = make_test_db(
        r#"
            library L {
                bytes32 internal constant SLOT = keccak256("x");
                bytes32 private constant HIDDEN = keccak256("y");
            }
            contract C {
                function f() public returns (bytes32) { return L.SLOT; }
                function g() public returns (bytes32) { return L.HIDDEN; }
            }
            "#,
    );
    let internal = named_function_inference(&db, file, "f");
    assert!(
        internal.diagnostics.is_empty(),
        "{:?}",
        codes(&internal)
    );
    let private = named_function_inference(&db, file, "g");
    assert!(
        codes(&private).contains(&DiagnosticCode::UnresolvedMember),
        "a private library constant must stay invisible: {:?}",
        codes(&private)
    );
}

/// `msg.value` inside a library function observes the caller's attached
/// value — the library runs in the caller's context via delegatecall, so
/// the payable gate does not apply. A contract's non-payable function still
/// errors.
#[test]
fn msg_value_is_allowed_in_library_functions() {
    let (db, file) = make_test_db(
        r#"
            library L {
                function check() internal returns (uint) { return msg.value; }
            }
            contract C {
                function f() public returns (uint) { return msg.value; }
            }
            "#,
    );
    let library = named_function_inference(&db, file, "check");
    assert!(
        library.diagnostics.is_empty(),
        "library functions observe the caller's msg.value: {:?}",
        codes(&library)
    );
    let contract = named_function_inference(&db, file, "f");
    assert!(contract.diagnostics.iter().any(|diagnostic| {
        matches!(diagnostic, SemanticDiagnostic::InferenceFailure { message, .. }
            if message.contains("payable"))
    }));
}

/// A getter and a bare-call address member both convert to their external
/// function-pointer types — `this.x` fills a `function() external view`
/// pointer (with `.selector`/`.address` intact), and `a.call` fills
/// `function(bytes memory) external payable returns (bool, bytes memory)`
/// because `TyName::Located` preserved the written `bytes memory`.
#[test]
fn members_convert_to_external_function_pointers() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                uint public x;
                function f() external view returns (uint) {
                    function () external view returns (uint) p = this.x;
                    bytes4 selector = this.x.selector;
                    address target = this.x.address;
                    return p() + uint32(selector) + uint160(target);
                }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "f");
    assert!(
        inference.diagnostics.is_empty(),
        "unexpected: {:?}",
        codes(&inference)
    );

    let (db, file) = make_test_db(
        "contract C { function f(address a, bytes memory m) external { \
         function(bytes memory) external payable returns (bool, bytes memory) p = a.call; \
         (bool ok, ) = p(m); \
         function(bytes memory) external view returns (bool, bytes memory) q = a.staticcall; } }",
    );
    let inference = named_function_inference(&db, file, "f");
    assert!(inference.diagnostics.is_empty(), "{:?}", codes(&inference));
}

/// `bytesN` implicitly converts to `bytes` — `keccak256` returns `bytes32`,
/// which is a valid `bytes.concat` argument.
#[test]
fn fixed_bytes_implicitly_converts_to_bytes() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function f(address who, address asset, uint amount) external view returns (bytes memory) {
                    return bytes.concat(keccak256(abi.encode(who, asset, amount)));
                }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "f");
    assert!(
        inference.diagnostics.is_empty(),
        "unexpected: {:?}",
        inference
            .diagnostics
            .iter()
            .map(SemanticDiagnostic::code)
            .collect::<Vec<_>>()
    );
}

/// solc keeps the *source's* location on every reference-typed cast — a
/// `calldata` argument answers `calldata` — so `bytes calldata a = bytes(s);`
/// is how OZ's `ERC7739Utils` gets `bytes calldata buffer` out of a `string
/// calldata`. The rule covers `bytes`/`string`, slices (whose location rides
/// on the base), and arrays; the complement is that nothing converts *into*
/// `calldata` — a `memory` or `storage` source cannot fill a `calldata`
/// target.
#[test]
fn casts_keep_the_source_location() {
    let accepts = [
        // calldata sources — the cast result is `calldata` and fills `calldata`.
        r#"
            contract C {
                function f(string calldata s, bytes calldata b) external pure {
                    bytes calldata a = bytes(s);
                    string calldata c = string(b);
                    bytes calldata d = bytes(b);
                }
            }
            "#,
        // storage sources — the cast result is `storage`, so it binds a pointer.
        r#"
            contract C {
                bytes sb;
                function f() external {
                    bytes storage p = bytes(sb);
                    string storage q = string(sb);
                }
            }
            "#,
        // every cast, not only `bytes`/`string`: `T[](x)` on a `calldata`
        // array is `calldata`.
        r#"
            contract C {
                function f(uint256[] calldata a, uint256[4] calldata b) external pure {
                    uint256[] calldata x = uint256[](a);
                    uint256[] memory y = uint256[](a);
                    uint256[4] calldata z = uint256[4](b);
                    uint256[] calldata w = uint256[](a[0:2]);
                }
            }
            "#,
    ];
    for source in accepts {
        let (db, file) = make_test_db(source);
        let inference = named_function_inference(&db, file, "f");
        assert!(
            inference.diagnostics.is_empty(),
            "{source}: {:?}",
            codes(&inference)
        );
    }

    // A slice cast is OZ's `draft-ERC7739Utils`: `string(encodedSignature[a:b])`
    // fills a `string calldata` return.
    let (db, file) = make_test_db(
        r#"
            contract C {
                function f(bytes calldata b)
                    external
                    pure
                    returns (bytes calldata sig, string calldata descr)
                {
                    sig = bytes(b[:32]);
                    descr = string(b[32:]);
                }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "f");
    assert!(inference.diagnostics.is_empty(), "{:?}", codes(&inference));
    let calldata_string = expr_results(&inference).any(|(_, ty, _)| {
        matches!(
            ty.kind(&db),
            TyKind::Ref(inner, Location::Calldata)
                if matches!(inner.kind(&db), TyKind::Primitive(Primitive::String))
        )
    });
    assert!(
        calldata_string,
        "`string(slice)` should type as `string calldata`"
    );

    for source in [
        // memory source -> calldata target
        "contract C { function f(string memory s) external pure { bytes calldata bad = bytes(s); } }",
        "contract C { function f(bytes memory b) external pure { string calldata bad = string(b); } }",
        // storage source -> calldata target
        "contract C { bytes sb; function f() external { bytes calldata bad = bytes(sb); } }",
    ] {
        assert!(
            reports_type_mismatch(source, "f"),
            "{source} must report a mismatch"
        );
    }
}

/// References that are not function values: a declaration is not callable or
/// assignable (`C.f()` is "declaration-only", `take(C.f)` a mismatch); a
/// builtin name converts to nothing (it is `CallKind::Builtin`, resolved from
/// the table rather than reached through a declaration — only entries
/// callable through an instance, like `address.call`, convert); and a
/// type-namespace reference (`L.f`, `D.f`) is a pointer only for an *internal*
/// library function — a public library function is a delegatecall, not a
/// jump, and a contract declaration has no mobile type. The owner is captured
/// in the `ViaType` kind at resolution time, so `Fn::apply_call_kind` reads
/// it off the kind.
#[test]
fn references_that_are_not_function_values() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function f() external {}
                function take(function() external ptr) internal {}
                function callIt() external { C.f(); }
                function assignIt() external { take(C.f); }
            }
            "#,
    );
    let call = named_function_inference(&db, file, "callIt");
    assert!(call.diagnostics.iter().any(|d| {
        matches!(d, SemanticDiagnostic::InferenceFailure { message, .. }
            if message.contains("declaration-only"))
    }));
    let assign = named_function_inference(&db, file, "assignIt");
    assert!(
        assign
            .diagnostics
            .iter()
            .any(|d| { matches!(d, SemanticDiagnostic::TypeMismatch { .. }) })
    );

    let (db, file) = make_test_db(
        r#"contract C {
                function take(function(uint) internal returns (bytes32) ptr) internal {}
                function run() external { take(blockhash); }
            }"#,
    );
    let inference = named_function_inference(&db, file, "run");
    assert!(
        inference
            .diagnostics
            .iter()
            .any(|d| matches!(d, SemanticDiagnostic::TypeMismatch { .. })),
        "`blockhash` must not convert to a function pointer"
    );

    let cases: &[(&str, &str, bool, &str)] = &[
        (
            "library L { function f() internal pure returns (uint) { return 1; } }",
            "take(L.f)",
            false,
            "an internal library function is a pointer",
        ),
        (
            "library L { function f() public pure returns (uint) { return 1; } }",
            "take(L.f)",
            true,
            "a public library function is a delegatecall, not a pointer",
        ),
        (
            "contract D { function f() public pure returns (uint) { return 1; } }",
            "take(D.f)",
            true,
            "a contract declaration has no mobile type",
        ),
    ];
    for (decl, call, expect_mismatch, label) in cases {
        let source = format!(
            "contract C {{ function take(function() internal pure returns (uint) ptr) \
             internal {{}} function run() external {{ {call}; }} }} {decl}"
        );
        assert_eq!(
            reports_type_mismatch(&source, "run"),
            *expect_mismatch,
            "{label}"
        );
    }
}

/// The internal/external clause, as a matrix. The flavour belongs to the
/// *reference*, not to the declaration: `g` by name is an internal pointer
/// even when `g` is `public`, while `this.g` is an external one. The two
/// flavours are disjoint — neither substitutes for the other in either
/// direction, which is why the docs say "identical" rather than "more
/// restrictive".
#[test]
fn function_type_flavour_is_a_property_of_the_reference() {
    // (declared visibility, reference, fills an internal pointer, fills an external one)
    let cases: &[(&str, &str, bool, bool)] = &[
        ("private", "g", true, false),
        ("internal", "g", true, false),
        ("public", "g", true, false),
        ("public", "this.g", false, true),
        ("external", "this.g", false, true),
    ];
    for (vis, reference, fills_internal, fills_external) in cases {
        for (flavour, fills) in [("internal", fills_internal), ("external", fills_external)] {
            let source = format!(
                "contract C {{ function g() {vis} pure {{}} \
                 function take(function() {flavour} pure ptr) internal {{}} \
                 function f() external {{ take({reference}); }} }}"
            );
            assert_eq!(
                reports_type_mismatch(&source, "f"),
                !fills,
                "{vis} `{reference}` against a {flavour} pointer"
            );
        }
    }

    // A bare `g` for an `external` declaration is not a reference at all —
    // solc reports "Undeclared identifier", which is an unresolved name, not
    // a conversion failure.
    let (db, file) = make_test_db(
        "contract C { function g() external pure {} \
         function take(function() internal pure ptr) internal {} \
         function f() external { take(g); } }",
    );
    let inference = named_function_inference(&db, file, "f");
    assert!(
        inference
            .diagnostics
            .iter()
            .any(|d| matches!(d, SemanticDiagnostic::UnresolvedName { .. })),
        "a bare `external` function has no internal reference"
    );
    assert!(
        !inference
            .diagnostics
            .iter()
            .any(|d| matches!(d, SemanticDiagnostic::TypeMismatch { .. }))
    );
}

/// Solidity's function-type conversion rule, clause by clause. Every
/// rejecting case here used to be a silent false accept.
///
/// Note the shape of each case: the source is *passed to a parameter* of
/// the target function type. Arguments are the only conversion path the
/// inferer checks today — variable initializers are not checked at all
/// (`uint x = "abc";` is accepted), so an initializer would prove nothing
/// about this rule.
#[test]
fn function_type_conversions_follow_the_documented_rule() {
    let cases: &[(&str, &str, &str, bool, &str)] = &[
        (
            "function g(uint x) internal view returns (uint) { return x; }",
            "function(uint) internal pure returns (uint)",
            "take(g)",
            true,
            "`view` must not fill a `pure` pointer",
        ),
        (
            "function g(uint x) internal pure returns (uint) { return x; }",
            "function(uint) internal view returns (uint)",
            "take(g)",
            false,
            "`pure` may fill a `view` pointer",
        ),
        (
            "function g(uint x) internal pure returns (uint) { return x; }",
            "function(uint) external pure returns (uint)",
            "take(g)",
            true,
            "an internal reference must not fill an external pointer",
        ),
        (
            "function g(uint x) external pure returns (uint) { return x; }",
            "function(uint) external pure returns (uint)",
            "take(this.g)",
            false,
            "an external reference may fill an external pointer",
        ),
        (
            "function g(uint8 x) internal pure returns (uint) { return x; }",
            "function(uint) internal pure returns (uint)",
            "take(g)",
            true,
            "parameter types must be identical, not merely convertible",
        ),
        (
            "function g(uint x) internal pure returns (uint8) { return uint8(x); }",
            "function(uint) internal pure returns (uint)",
            "take(g)",
            true,
            "return types must be identical, not merely convertible",
        ),
    ];
    for (decl, param, call, expect_mismatch, label) in cases {
        let source = format!(
            "contract C {{ {decl} function take({param} ptr) internal {{}} \
             function f() external {{ {call}; }} }}"
        );
        assert_eq!(
            reports_type_mismatch(&source, "f"),
            *expect_mismatch,
            "{label}"
        );
    }

    // A `memory`/`calldata`/`storage` keyword inside a function-type parameter
    // or return survives lowering into the `Fn` signature, and solc compares
    // function-type parameters by exact type equality — so `memory`/`calldata`
    // is a clause of its own: an exact match is accepted, a mismatched
    // location is not (which makes a written `calldata` external pointer
    // effectively unfillable, since external *references* normalize to
    // `memory`), and internal pointers keep their locations too.
    let (db, file) = make_test_db(
        "contract C { function e(bytes memory b) external pure {} \
         function take(function(bytes memory) external pure ptr) internal {} \
         function f() external { take(this.e); } }",
    );
    let inference = named_function_inference(&db, file, "f");
    assert!(inference.diagnostics.is_empty(), "{:?}", codes(&inference));

    assert!(reports_type_mismatch(
        "contract C { function e(bytes memory b) external pure {} \
         function take(function(bytes calldata) external pure ptr) internal {} \
         function f() external { take(this.e); } }",
        "f"
    ));

    let (db, file) = make_test_db(
        "contract C { function g(string calldata s) internal pure returns (bytes memory) { return bytes(s); } \
         function take(function(string calldata) internal pure returns (bytes memory) ptr) internal {} \
         function f() external { take(g); } }",
    );
    let inference = named_function_inference(&db, file, "f");
    assert!(inference.diagnostics.is_empty(), "{:?}", codes(&inference));
}

/// `this.f{value: 1}` is a call in progress, not a value — outside a call's
/// callee position it is rejected outright (solc spells the same rejection as
/// a conversion error between identical-looking types), while through the
/// forwarded resolution and an external pointer's `FnPtr` the options
/// expression still reaches its call.
#[test]
fn call_options_reach_their_call_only() {
    let (db, file) = make_test_db(
        "contract C { function f() external payable {} \
         function g() external { \
             function() external payable p = this.f{value: 1}; } }",
    );
    let inference = named_function_inference(&db, file, "g");
    assert!(
        inference.diagnostics.iter().any(|d| matches!(d,
            SemanticDiagnostic::InferenceFailure { message, .. }
            if message.contains("options can only be set on a function call"))),
        "{:?}",
        codes(&inference)
    );

    let (db, file) = make_test_db(
        "contract C { function f(bytes memory m) external payable {} \
         function g(bytes memory m, function(bytes memory) external payable p) external payable { \
             this.f{value: 1}(m); p{value: 1}(m); } }",
    );
    let inference = named_function_inference(&db, file, "g");
    assert!(inference.diagnostics.is_empty(), "{:?}", codes(&inference));
}

/// `address.call`'s pointer signature is `function(bytes memory) external
///
/// Inside a function type, locations are validated like a parameter's: a
/// location on a non-reference element is an `InvalidDataLocation`, and a
/// reference-typed parameter or return *requires* one — omitting it reports
/// `DataLocationRequired` and poisons the pointer rather than leaking an
/// unlocated `Struct`/`Array` into expression inference.
#[test]
fn function_type_locations_are_validated() {
    let (db, file) = make_test_db(
        "contract C { function f(function(uint memory) internal ptr) internal {} }",
    );
    let inference = named_function_inference(&db, file, "f");
    assert!(
        inference
            .diagnostics
            .iter()
            .any(|d| matches!(d, SemanticDiagnostic::InvalidDataLocation { .. })),
        "`uint memory` inside a function type must be rejected: {:?}",
        codes(&inference)
    );

    let cases = [
        "contract C { function f(function(bytes) internal ptr) internal {} }",
        "contract C { struct S { uint[] a; } function f() internal { function() internal returns (S) fp; fp().a[0]; } }",
    ];
    for source in cases {
        let (db, file) = make_test_db(source);
        let inference = named_function_inference(&db, file, "f");
        assert!(
            inference
                .diagnostics
                .iter()
                .any(|d| matches!(d, SemanticDiagnostic::DataLocationRequired { .. })),
            "{source} must report DataLocationRequired: {:?}",
            codes(&inference)
        );
        assert!(
            !expr_results(&inference)
                .any(|(_, ty, _)| matches!(ty.kind(&db), TyKind::Ref(_, Location::Stack))),
            "{source} must never mint a stack-located Ref"
        );
    }
}

/// An internal function *is* a value: `Function` is not nameable, so it can
/// never be a conversion *target*, but it converts *to* a compatible pointer.
/// A base-qualified reference converts only when it names an implemented
/// internal-call target — a bodyless declaration retains a declaration type
/// and must not become an internal function pointer.
#[test]
fn function_values_convert_to_function_pointers() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function g(uint x) internal pure returns (uint) { return x; }
                function take(function(uint) internal pure returns (uint) ptr) internal {}
                function assignIt() external {
                    function(uint) internal pure returns (uint) fp = g;
                }
                function passIt() external { take(g); }
            }
            "#,
    );
    for name in ["assignIt", "passIt"] {
        let inference = named_function_inference(&db, file, name);
        assert!(
            inference.diagnostics.is_empty(),
            "{name}: {:?}",
            codes(&inference)
        );
    }

    let (db, file) = make_test_db(
        r#"
            abstract contract C {
                function implemented() public pure returns (uint) { return 1; }
                function declared() public pure returns (uint);
                function take(function() internal pure returns (uint) ptr) internal {}
                function good() external { take(C.implemented); }
                function bad() external { take(C.declared); }
            }
            "#,
    );
    let good = named_function_inference(&db, file, "good");
    assert!(good.diagnostics.is_empty(), "{:?}", codes(&good));

    let bad = named_function_inference(&db, file, "bad");
    assert!(
        bad.diagnostics
            .iter()
            .any(|diagnostic| matches!(diagnostic, SemanticDiagnostic::TypeMismatch { .. })),
        "a bodyless contract declaration must not convert to an internal pointer: {:?}",
        codes(&bad)
    );
}

/// `=` lowers with `op == None`; `op=` keeps its base binary operator,
/// including the two-character shifts. An assignment types as its target, so
/// a chained assignment and a target-typed expression both land on the
/// target's type.
#[test]
fn assignments_lower_with_their_operator_and_type_as_the_target() {
    let (db, file) =
        make_test_db("contract C { function f() external { uint x; x = 1; x += 2; x <<= 3; } }");
    let def = find_function(&db, file, "f");
    let ops: Vec<Option<BinaryOp>> = assignments(&db, def)
        .into_iter()
        .map(|(_, op, _)| op)
        .collect();
    assert_eq!(ops, vec![None, Some(BinaryOp::Add), Some(BinaryOp::Shl)]);

    let (db, file) =
        make_test_db("contract C { function f() external { uint x; uint y; x = y; } }");
    let def = find_function(&db, file, "f");
    let inference = named_function_inference(&db, file, "f");
    assert!(inference.diagnostics.is_empty(), "{:?}", codes(&inference));
    let assigned = assignments(&db, def);
    assert_eq!(assigned.len(), 1);
    let (lhs, _, ty) = assigned[0];
    assert!(matches!(
        ty.kind(&db),
        TyKind::Primitive(Primitive::Uint(256))
    ));
    assert!(matches!(
        inference.expr_resolution(lhs),
        Some(Resolution::Local(_))
    ));

    let (db, file) =
        make_test_db("contract C { function f() external { uint x; uint y; y = (x = 1); } }");
    let def = find_function(&db, file, "f");
    let inference = named_function_inference(&db, file, "f");
    assert!(inference.diagnostics.is_empty(), "{:?}", codes(&inference));
    let assigned = assignments(&db, def);
    assert_eq!(assigned.len(), 2);
    assert!(
        assigned
            .iter()
            .all(|(_, _, ty)| matches!(ty.kind(&db), TyKind::Primitive(Primitive::Uint(256))))
    );
}

/// Every target shape that is assignable: a local, a state variable, a mapping
/// element, an array element, a struct field, and a derived reference from a
/// storage pointer.
#[test]
fn assignments_that_are_accepted() {
    let fixtures = [
        r#"
            struct S { uint x; }
            contract C {
                function f() external {
                    uint[] memory a;
                    a[0] = 1;
                    S memory s;
                    s.x = 1;
                }
            }
            "#,
        r#"
            contract C {
                uint public count;
                mapping(address => uint) balances;
                function f(address a) external {
                    count = 1;
                    balances[a] = 2;
                }
            }
            "#,
        r#"
            struct S { string name; }
            contract C {
                string stored;
                uint[] storedArray;
                string[] storedStrings;
                S storedStruct;
                function f(string calldata c, uint[] calldata a) external {
                    string memory ms = "abc";
                    stored = ms;
                    stored = "literal";
                    string memory msCopy = stored;
                    storedArray = a;
                    uint[] memory aCopy = storedArray;
                    storedStrings[0] = ms;
                    storedStruct.name = ms;
                    string storage p = stored;
                    p = stored;
                }
            }
            "#,
        r#"
            contract C {
                string stored;
                function g(string storage s) internal {}
                function f(string memory m, string storage other) internal {
                    stored = m;
                    g(other);
                }
            }
            "#,
        r#"
            struct S { string name; }
            contract C {
                S stored;
                string[] storedStrings;
                function f(string memory m) external {
                    S storage p = stored;
                    p.name = m;
                    string[] storage q = storedStrings;
                    q[0] = m;
                }
            }
            "#,
        // Tuple destructuring: a hole is an ignored slot, and the arity counts
        // it — solc accepts all of these and rejects a hole in a *value*
        // position.
        r#"
            contract C {
                function g() internal pure returns (uint, uint, uint) { return (1, 2, 3); }
                function f() external {
                    uint x; uint y;
                    (x, y) = (1, 2);
                    (x, , y) = g();
                    (x, y,) = (1, 2, 3);
                    (,x) = (1, 2);
                }
            }
            "#,
        // `(x)` is not a real tuple, so parentheses peel — including around a
        // tuple: `((x, y))` is `(x, y)`.
        r#"
            contract C {
                function f() external pure {
                    uint x; uint y;
                    ((x, y)) = (1, 2);
                    ((x)) = 1;
                    (x, y) = ((1, 2));
                }
            }
            "#,
    ];
    for source in fixtures {
        let (db, file) = make_test_db(source);
        let inference = named_function_inference(&db, file, "f");
        assert!(
            inference.diagnostics.is_empty(),
            "{source}: {:?}",
            codes(&inference)
        );
    }
}

/// Every target shape that is rejected: a non-lvalue, a storage *pointer* (a
/// local or a storage parameter) from a `memory` source, a `calldata` target,
/// and a value that does not convert.
#[test]
fn assignments_that_are_rejected() {
    fn not_assignable(d: &SemanticDiagnostic) -> bool {
        matches!(d, SemanticDiagnostic::InferenceFailure { message, .. }
            if message.contains("cannot be assigned to"))
    }
    fn mismatch(d: &SemanticDiagnostic) -> bool {
        matches!(d, SemanticDiagnostic::TypeMismatch { .. })
    }

    let rejects: Rejects = &[
        (
            "contract C { function f() external { 1 = 2; } }",
            not_assignable,
        ),
        (
            "contract C { function g() internal pure returns (uint) { return 1; } function f() external { g() = 2; } }",
            not_assignable,
        ),
        (
            "contract C { function f(uint[] memory a) external { a.length = 1; } }",
            not_assignable,
        ),
        // local storage pointer, assigned
        (
            r#"
            contract C {
                string stored;
                function f(string memory m) external { string storage p = stored; p = m; }
            }
            "#,
            mismatch,
        ),
        // local storage pointer, initialized
        (
            "contract C { function f(string memory m) external { string storage p = m; } }",
            mismatch,
        ),
        // storage parameter — a local, therefore a pointer
        (
            r#"
            contract C {
                function g(string storage s) internal {}
                function f(string memory m) internal { g(m); }
            }
            "#,
            mismatch,
        ),
        // no implicit conversion into `calldata`
        (
            "contract C { function f(string calldata c, string memory m) external { c = m; } }",
            mismatch,
        ),
        (
            "contract C { function f() external { uint x; x = \"abc\"; } }",
            mismatch,
        ),
        // Tuple destructuring arity — the hole counts, so `(x, , y)` needs a
        // three-component source (solc: "not implicitly convertible").
        (
            r#"
            contract C {
                function g2() internal pure returns (uint, uint) { return (1, 2); }
                function f() external pure { uint x; uint y; (x, , y) = g2(); }
            }
            "#,
            mismatch,
        ),
        (
            r#"
            contract C {
                function g3() internal pure returns (uint, uint, uint) { return (1, 2, 3); }
                function f() external pure { uint x; uint y; (x, y) = g3(); }
            }
            "#,
            mismatch,
        ),
        // `constant` state is read-only
        (
            "contract C { uint constant K = 1; function f() external { K = 2; } }",
            not_assignable,
        ),
    ];
    for (source, expected) in rejects {
        let (db, file) = make_test_db(source);
        let inference = named_function_inference(&db, file, "f");
        assert!(
            inference.diagnostics.iter().any(expected),
            "{source}: {:?}",
            codes(&inference)
        );
    }
}

/// The initializer check reuses `implicitly_converts_to`'s located/ABI
/// asymmetry, so reference-typed initializers must still convert — and a
/// genuine mismatch must report.
#[test]
fn initializers_are_conversion_checked() {
    let accepts = [
        r#"contract C { function f() external { uint8 x = 1; uint256 y = x; } }"#,
        r#"contract C { function f() external pure { string memory s = "abc"; } }"#,
        r#"contract C { function f() external pure { bytes memory b = abi.encode(1); } }"#,
        r#"contract C { function f() external pure { uint[] memory a = abi.decode(abi.encode(1), (uint[])); } }"#,
        r#"contract C { function f(uint[] memory a) external pure { uint[] memory b = a; } }"#,
        r#"contract C { function f() external pure { bytes memory b = "abc"; } }"#,
        r#"contract C { function f() external pure { address a = address(0); } }"#,
        r#"contract C { function f() external view { bytes memory b = abi.encodePacked(1); bytes32 h = keccak256("x"); address a = msg.sender; uint n = block.number; } }"#,
        r#"contract C { function f(address payable p) external pure { address a = p; } }"#,
    ];
    for source in accepts {
        let (db, file) = make_test_db(source);
        let inference = named_function_inference(&db, file, "f");
        assert!(
            inference.diagnostics.is_empty(),
            "{source}: {:?}",
            codes(&inference)
        );
    }

    let rejects = [
        "contract C { function f() external { uint x = \"abc\"; } }",
        "contract C { function f(uint[] memory a) external pure { bytes memory b = a; } }",
        // A call result is checked like any other value, on initialization
        // and assignment alike.
        "contract C { function t() internal pure returns (string memory) { return \"\"; } \
         function f() external { uint x = t(); } }",
        "contract C { function t() internal pure returns (string memory) { return \"\"; } \
         function f() external { uint x; x = t(); } }",
    ];
    for source in rejects {
        let (db, file) = make_test_db(source);
        let inference = named_function_inference(&db, file, "f");
        assert!(
            inference
                .diagnostics
                .iter()
                .any(|d| matches!(d, SemanticDiagnostic::TypeMismatch { .. })),
            "{source}: {:?}",
            codes(&inference),
        );
    }
}

// ── Explicit conversions (casts) ─────────────────────────────────────────

/// One parameter per kind the explicit relation branches on. A cast is the
/// only way past the implicit relation, so `run_cast` reports whether solc
/// accepts it.
const CAST_FIXTURE: &str = r#"
    enum E { A, B }
    enum Five { A, B, C, D, E }
    type U is uint256;
    interface Iface { }
    library Lib { function g() internal { } }
    contract C { }
    contract PC { receive() external payable { } }
    contract FallbackBase { fallback() external payable { } }
    contract PayableFallback is FallbackBase { }
    contract User {
        function f(
            uint256 u, uint8 u8, int8 i8, int16 i16, int256 i256, uint160 u160,
            address a, address payable ap, bytes32 b32, bytes20 b20, bytes3 b3, bytes1 b1,
            bytes memory bs, string memory s, C c, PC pc, PayableFallback pfb, E e, U ud
        ) external {
            @BODY@;
        }
    }
"#;

/// The diagnostic codes `BODY;` produces inside the fixture. Empty means the
/// cast was accepted.
fn run_cast(body: &str) -> Vec<DiagnosticCode> {
    let (db, file) = make_test_db(&CAST_FIXTURE.replace("@BODY@", body));
    named_function_inference(&db, file, "f")
        .diagnostics
        .iter()
        .map(SemanticDiagnostic::code)
        .collect()
}

/// The explicit-conversion table, pinned against solc 0.8.34 case by case.
///
/// The relation **defaults to the implicit one**, so each entry is what a cast
/// adds over an assignment — or refuses to add. Grouped by solc's own rule
/// (the `isExplicitlyConvertibleTo` override each case comes from).
#[test]
fn explicit_conversions_match_solc() {
    let cases: &[(&str, bool)] = &[
        // `IntegerType`: same width, or same signedness.
        ("uint256(u8)", true),
        ("uint8(u)", true),
        ("int8(u8)", true),
        ("uint8(i8)", true),
        ("uint256(i256)", true),
        ("uint8(i16)", false),
        ("uint160(u)", true),
        // `AddressType`/`IntegerType`: 160 bits, unsigned, non-payable only.
        ("address(u160)", true),
        ("address(u)", false),
        ("uint160(a)", true),
        ("uint256(a)", false),
        ("uint160(ap)", false),
        ("address(ap)", true),
        ("payable(a)", true),
        // `IntegerType`/`FixedBytesType`: matching width, unsigned only.
        ("bytes32(u)", true),
        ("bytes3(u)", false),
        ("uint256(b32)", true),
        ("int256(b32)", false),
        ("int8(b1)", false),
        ("bytes1(i8)", false),
        ("bytes1(b32)", true),
        ("address(b20)", true),
        ("bytes20(a)", true),
        ("address(b3)", false),
        // `BoolType` has no override: a bool is only itself.
        ("bool(u8)", false),
        // `IntegerType`/`EnumType`: an enum takes any integer, and reaches only
        // unsigned integers.
        ("uint8(e)", true),
        ("int8(e)", false),
        ("E(u8)", true),
        ("E(i8)", true),
        // `ArrayType`: a `bytes`/`string` pair converts both ways, and only
        // `bytes` reaches `bytesN`.
        ("string(bs)", true),
        ("bytes(s)", true),
        ("bytes32(bs)", true),
        ("bytes32(s)", false),
        // `bytesN` converts to `bytes` implicitly — the cast also compiles.
        ("bytes(b32)", true),
        ("bytes(b1)", true),
        // `ContractType`/`AddressType`: contract payability gates conversions
        // to and from `address payable`, but not `address`.
        ("address(c)", true),
        ("address(pc)", true),
        ("payable(c)", false),
        ("payable(pc)", true),
        ("payable(pfb)", true),
        ("PayableFallback(a)", false),
        ("uint160(c)", false),
        ("C(a)", true),
        ("C(u)", false),
        // `TypeType`: a library's *type name* reaches a non-payable address.
        ("address(Lib)", true),
        ("payable(Lib)", false),
        ("uint160(Lib)", false),
        // `UserDefinedValueType` has no override: it is only itself.
        ("U(ud)", true),
        ("U(u)", false),
        ("uint256(ud)", false),
        // `RationalNumberType`: zero is the one literal that reaches a payable
        // address or a fixed-bytes type; otherwise an unsigned literal within
        // 160 bits reaches a non-payable address.
        ("address(0)", true),
        ("address(1)", true),
        ("address(1 ether)", true),
        ("payable(0)", true),
        ("payable(1)", false),
        ("bytes32(0)", true),
        ("bytes32(1)", false),
        ("uint8(300)", false),
        // A *negative* literal cannot be written here yet — unary minus is not
        // lowered, so `-1` arrives as `1`; `negative_literals_do_not_reach_
        // unsigned_targets` pins that rule on the relation directly.
        // An enum takes a non-negative literal within its member count.
        ("E(0)", true),
        ("E(1)", true),
        ("E(5)", false),
        // The relation's rule is a sound *lower bound* (a literal's type keeps
        // only its magnitude's bit length), so `Five(5)` is the case only the
        // call site can decide — it still has the argument expression.
        ("Five(4)", true),
        ("Five(5)", false),
        // `StringLiteralType` has no override: a hex literal is its own.
        ("bytes1(hex\"ff\")", true),
        ("uint8(hex\"ff\")", false),
    ];

    for (cast, expected) in cases {
        let codes = run_cast(cast);
        assert_eq!(
            codes.is_empty(),
            *expected,
            "`{cast}` should be {}, got {codes:?}",
            if *expected { "accepted" } else { "rejected" },
        );
    }
}

/// `0` is the one integer literal every `bytesNN` accepts (`Memory.asPointer(0x00)`
/// and `return (…, 0, 0, …)` into `bytes32` returns, OZ). A nonzero literal
/// converts only at its exact byte width.
#[test]
fn integer_literals_convert_to_fixed_bytes_only_at_zero_or_exact_width() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function g(bytes32 b) internal pure {}
                function h(bytes20 b) internal pure {}
                function f() internal pure returns (bytes32) {
                    bytes32 x = 0x00;
                    g(0);
                    h(0x0);
                    return x;
                }
            }
            "#,
    );
    let f = named_function_inference(&db, file, "f");
    assert!(f.diagnostics.is_empty(), "{:?}", codes(&f));

    let (db, file) = make_test_db(
        "contract C { function g(bytes32 b) internal pure {} function f() internal { g(1); g(0xff); } }",
    );
    let f = named_function_inference(&db, file, "f");
    assert_eq!(
        f.diagnostics
            .iter()
            .filter(|d| matches!(d, SemanticDiagnostic::TypeMismatch { .. }))
            .count(),
        2,
        "{:?}",
        codes(&f),
    );
}

/// No nested `Ref`: a compound's interiors are canonical, so `Ref` appears
/// once, at the top of the located slot — never inside `Array { elem }` or
/// `Mapping { value }`.
#[test]
fn located_slots_never_nest_references() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                mapping(uint256 => uint256[]) public m;
                function f(string[] memory a) internal {}
            }
            "#,
    );

    let inference = named_function_inference(&db, file, "f");
    let local = inference
        .locals
        .iter()
        .map(|(_, ty)| *ty)
        .next()
        .expect("the parameter has a type");
    let TyKind::Ref(array, Location::Memory) = local.kind(&db) else {
        panic!("expected `string[] memory`, got `{}`", local.display(&db));
    };
    let TyKind::Array { elem, .. } = array.kind(&db) else {
        panic!("expected an array inside the reference, got `{}`", array.display(&db));
    };
    assert!(
        matches!(elem.kind(&db), TyKind::Primitive(Primitive::String)),
        "the element must be canonical, got `{}`",
        elem.display(&db)
    );

    let var = db
        .file_def_map(file)
        .defs
        .keys()
        .find_map(|def| match def {
            DefId::Var(id) if db.var_data(*id).name == "m" => Some(*id),
            _ => None,
        })
        .expect("`m` is declared");
    let mapping = var_inference(&db, file, "m").ty(db.var_data(var).ty_ref);
    let TyKind::Mapping { key, value } = mapping.peel_loc(&db).kind(&db) else {
        panic!("expected an unlocated mapping, got `{}`", mapping.display(&db));
    };
    assert!(
        matches!(key.kind(&db), TyKind::Primitive(Primitive::Uint(256))),
        "the key must be canonical, got `{}`",
        key.display(&db)
    );
    let TyKind::Array { elem, .. } = value.kind(&db) else {
        panic!("expected a canonical array value, got `{}`", value.display(&db));
    };
    assert!(
        matches!(elem.kind(&db), TyKind::Primitive(Primitive::Uint(256))),
        "the value's element must be canonical, got `{}`",
        elem.display(&db)
    );

    // `bytes("abc")` in constant evaluation and `"a" : "b"` ternary deduction
    // both work with default-located `bytes`/`string`.
    let (db, file) = make_test_db(
        r#"
            contract C {
                function g(bool cond) external pure returns (string memory) {
                    uint[bytes("abc").length] memory arr;
                    arr;
                    return cond ? "a" : "b";
                }
            }
            "#,
    );
    let g = named_function_inference(&db, file, "g");
    assert!(g.diagnostics.is_empty(), "{:?}", codes(&g));
}
