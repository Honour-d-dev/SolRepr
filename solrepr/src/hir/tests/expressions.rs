use super::*;

/// The grammar's `optional(index)` exists for `T[]`, so `a[]` is malformed.
#[test]
fn value_array_access_without_an_index_is_reported() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function f() external {
                    uint256[] memory a;
                    a[];
                }
            }
            "#,
    );
    let inference = function_inference(&db, file);
    assert!(
        inference.diagnostics.iter().any(|diagnostic| matches!(
            diagnostic,
            SemanticDiagnostic::InferenceFailure { message, .. }
                if message.contains("requires an index")
        )),
        "{:?}",
        inference
            .diagnostics
            .iter()
            .map(SemanticDiagnostic::code)
            .collect::<Vec<_>>(),
    );
}

#[test]
fn value_tuples_produce_a_tuple_type() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function f(uint256 a, uint256 b) external {
                    (a, b);
                }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "f");
    assert!(
        inference.diagnostics.is_empty(),
        "{:?}",
        inference
            .diagnostics
            .iter()
            .map(SemanticDiagnostic::code)
            .collect::<Vec<_>>()
    );
    assert!(
        expr_results(&inference).any(|(_, ty, _)| {
            matches!(ty.kind(&db), TyKind::Tuple(tuple) if tuple.elements.len() == 2)
        }),
        "expected `(a, b)` to type as a two-element tuple"
    );
}

/// A component keeps the type it infers to — literals included. solc accepts
/// `(1.5, 2)` and types it `tuple(rational_const 3/2, int_const 2)`, so a
/// non-integral rational is an ordinary component, not a dropped one. Keeping
/// the literal keeps its conversions too: `(b, c) = (0x12345678, 1)` needs
/// `bytes4`, `(bb, c) = ("abc", 1)` needs `bytes`, `(a, c) = (0x…01, 1)`
/// needs `address` — narrowing to the smallest type loses all three.
#[test]
fn tuple_components_keep_their_literal_types() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function f() external pure {
                    (1.5, 2);
                }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "f");
    assert!(inference.diagnostics.is_empty(), "{:?}", codes(&inference));
    let kept_literal = expr_results(&inference).any(|(_, ty, _)| {
        let TyKind::Tuple(tuple) = ty.kind(&db) else {
            return false;
        };
        tuple
            .elements
            .iter()
            .any(|elem| matches!(elem, Some(ty) if matches!(ty.kind(&db), TyKind::Literal(_))))
    });
    assert!(
        kept_literal,
        "the rational component should be kept as a literal"
    );

    let (db, file) = make_test_db(
        r#"
            contract C {
                function hex4() external pure { bytes4 b; uint c; (b, c) = (0x12345678, 1); }
                function str() external pure { bytes memory bb; uint c; (bb, c) = ("abc", 1); }
                function addr() external pure {
                    address a; uint c;
                    (a, c) = (0x0000000000000000000000000000000000000001, 1);
                }
            }
            "#,
    );
    for name in ["hex4", "str", "addr"] {
        let inference = named_function_inference(&db, file, name);
        assert!(
            inference.diagnostics.is_empty(),
            "{name}: {:?}",
            codes(&inference)
        );
    }
}

#[test]
fn meta_type_requires_a_ty_ref() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function ok() external pure returns (uint256) { return type(uint256).max; }
                function bad(uint256 x) external pure { type(x); }
            }
            "#,
    );
    let ok = named_function_inference(&db, file, "ok");
    assert!(
        ok.diagnostics.is_empty(),
        "{:?}",
        ok.diagnostics
            .iter()
            .map(SemanticDiagnostic::code)
            .collect::<Vec<_>>()
    );
    let bad = named_function_inference(&db, file, "bad");
    assert!(bad.diagnostics.iter().any(|d| matches!(d,
        SemanticDiagnostic::InferenceFailure { message, .. }
        if message.contains("type name"))));
}

#[test]
fn meta_enum_min_max_are_the_enum_type() {
    // `type(E).min`/`.max` are `E` values, not `uint8`.
    let (db, file) = make_test_db(
        r#"
            contract C {
                enum E { A, B }
                function f() external pure { E lo = type(E).min; }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "f");
    assert!(
        expr_results(&inference).any(|(_, ty, resolution)| {
            matches!(resolution, Some(Resolution::Field(_)))
                && matches!(ty.kind(&db), TyKind::Enum(_))
        }),
        "expected `type(E).min` to type as the enum"
    );
}

#[test]
fn indexed_operands_bind_tighter_than_binary_operators() {
    // Subscript binds tighter than `==`, so both operands are
    // `array_access` and the comparison types as `bool`.
    let (db, file) = make_test_db(
        r#"
            contract C {
                function f(address[] memory _newOrder) external {
                    uint256 i = 0;
                    uint256 j = 1;
                    if (_newOrder[i] == _newOrder[j]) {}
                }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "f");
    assert!(
        inference.diagnostics.is_empty(),
        "{:?}",
        inference
            .diagnostics
            .iter()
            .map(SemanticDiagnostic::code)
            .collect::<Vec<_>>()
    );
    let condition = expr_results(&inference)
        .any(|(_, ty, _)| matches!(ty.kind(&db), TyKind::Primitive(Primitive::Boolean)));
    assert!(condition, "expected `a[i] == a[j]` to type as bool");
}

/// `(a, , b)` lowers to three components, the middle one omitted: the holes
/// are the grammar's *anonymous commas*, which `named_children()` drops.
#[test]
fn tuple_holes_are_lowered_as_omitted_components() {
    let (db, file) =
        make_test_db("contract C { function f() external pure { uint a; uint b; (a, , b); } }");
    let DefId::Function(id) = find_function(&db, file, "f") else {
        panic!("fixture declares function f")
    };
    let body = db.body_map(BodyOwnerId::Function(id));
    let components = body
        .syntax_store
        .exprs
        .iter()
        .find_map(|(_, expr)| match expr {
            Expr::Tuple(elems) => Some(elems.iter().map(Option::is_some).collect::<Vec<_>>()),
            _ => None,
        })
        .expect("the fixture has a tuple expression");
    assert_eq!(
        components,
        [true, false, true],
        "`(a, , b)` has three components"
    );
}

/// A hole is legal only in lvalue position, where it is an ignored slot and
/// the arity counts it — `(x, , y)` is a three-component target, `(x,)` a
/// two-component one. In a value position a hole denotes nothing, whether
/// top-level, trailing, or nested, and a valueless component counts as empty
/// too (solc: "Tuple component cannot be empty").
#[test]
fn empty_tuple_components_are_lvalue_only() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function v() internal pure {}
                function ok() external pure {
                    uint x; uint y;
                    (x, , y) = (1, 2, 3);
                    (x,) = (1, 2);
                }
                function bad() external pure { (1, , 2); }
                function trailing() external pure { uint x; (x,) = (1,); }
                function nested() external pure { (1, (2, ), 3); }
                function valueless() external pure { (v(), 1); }
            }
            "#,
    );
    let ok = named_function_inference(&db, file, "ok");
    assert!(ok.diagnostics.is_empty(), "{:?}", codes(&ok));
    for name in ["bad", "trailing", "nested", "valueless"] {
        let inference = named_function_inference(&db, file, name);
        assert!(
            inference.diagnostics.iter().any(|d| matches!(d,
                SemanticDiagnostic::InferenceFailure { message, .. }
                    if message.contains("tuple component cannot be empty"))),
            "{name}: {:?}",
            codes(&inference),
        );
    }
}

/// `(x)` is not a real tuple — parentheses peel, including around a tuple —
/// while `(x,)` is a real 2-component tuple (solc's parser: "`(x)` is not a
/// real tuple, `(x,)` is one-dimensional tuple").
#[test]
fn parenthesised_expressions_are_not_tuples() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function f() external pure returns (uint, uint) {
                    uint x; uint y;
                    ((x)) = 1;
                    (x, y) = ((1, 2));
                    return ((1, 2));
                }
            }
            "#,
    );
    let f = named_function_inference(&db, file, "f");
    assert!(f.diagnostics.is_empty(), "{:?}", codes(&f));
}

/// A tuple component is any expression's type: a nested tuple, a type name
/// beside a value, `msg`, a cast's runtime result (which stores
/// `Resolution::Type` but infers a value type). solc accepts all of these —
/// it types `(1, (2, 3))` as `tuple(int_const 1, tuple(int_const 2, int_const
/// 3))`.
#[test]
fn tuple_components_are_unrestricted_expressions() {
    let (db, file) = make_test_db(
        r#"
            struct S { uint256 x; }
            contract C {
                function nested() external pure { (1, (2, 3)); }
                function nested_target() external pure {
                    uint x; uint y; uint z;
                    ((x, y), z) = ((1, 2), 3);
                }
                function mixed() external view { (uint, 1); (msg, 1); }
                function casts(address a) external pure returns (bool, address) {
                    (true, S(1));
                    return (false, address(0));
                }
            }
            "#,
    );
    for name in ["nested", "nested_target", "mixed", "casts"] {
        let inference = named_function_inference(&db, file, name);
        assert!(
            inference.diagnostics.is_empty(),
            "{name}: {:?}",
            codes(&inference)
        );
    }
}

/// A qualified path is the grammar's hidden, flat `_identifier_path`
/// (`dotSep1(identifier)`), so `Base.m` arrives as two sibling identifiers and
/// must be folded into the member chain — otherwise it reads as the cast
/// `Base()`. This is what makes the *modifier* half of the invocability rule
/// observable: a contract/base modifier is reachable, a library's is not (solc:
/// "Can only use modifiers defined in the current contract or in base
/// contracts").
#[test]
fn qualified_modifier_paths_fold_into_the_member_chain() {
    let (db, file) = make_test_db(
        r#"
            library L { modifier m() { _; } }
            contract Base { modifier m() { _; } }
            contract C is Base {
                function base_mod() public Base.m {}
                function self_mod() public m {}
                function lib_mod() public L.m {}
            }
            "#,
    );
    for name in ["base_mod", "self_mod"] {
        let inference = named_function_inference(&db, file, name);
        assert!(
            inference.diagnostics.is_empty(),
            "{name}: {:?}",
            codes(&inference)
        );
    }
    let lib_mod = named_function_inference(&db, file, "lib_mod");
    assert!(
        !lib_mod.diagnostics.is_empty(),
        "a library's modifier must not be invocable"
    );
}

/// A qualified `using` source is the same flat path. Without the fold
/// `using {L.inc} for uint` is silently unattached, so `x.inc()` never resolves
/// — the common `using {SafeMath.add} for uint` form.
#[test]
fn qualified_using_sources_fold_into_the_member_chain() {
    let (db, file) = make_test_db(
        r#"
            library L { function inc(uint x) internal pure returns (uint) { return x + 1; } }
            contract C {
                using {L.inc} for uint;
                function f() external pure returns (uint) {
                    uint x = 1;
                    return x.inc();
                }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "f");
    assert!(inference.diagnostics.is_empty(), "{:?}", codes(&inference));
}

/// `payable(x)` is a **cast** to `address payable`, not its own lowering
/// category — so it lowers to the same `Expr::Call` shape as a
/// `type_cast_expression` and shares its result type (and, once cast argument
/// validation lands, its checks; solc reports `payable()`'s arity as "Exactly
/// one argument expected for explicit type conversion"). The `payable` keyword
/// is anonymous in the grammar, unlike a cast's `primitive_type`, so the callee
/// ident is synthesized over it.
#[test]
fn payable_is_a_cast_to_address_payable() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function to_payable(address x) external pure returns (address payable) {
                    return payable(x);
                }
                function to_address(address x) external pure returns (address) {
                    return payable(x);
                }
                function bad(address x) external pure returns (uint) {
                    return payable(x);
                }
            }
            "#,
    );
    for name in ["to_payable", "to_address"] {
        let inference = named_function_inference(&db, file, name);
        assert!(
            inference.diagnostics.is_empty(),
            "{name}: {:?}",
            codes(&inference)
        );
    }
    let bad = named_function_inference(&db, file, "bad");
    assert!(
        bad.diagnostics
            .iter()
            .any(|d| matches!(d, SemanticDiagnostic::TypeMismatch { .. })),
        "{:?}",
        codes(&bad),
    );
}

/// The type the fixture's single ternary inferred to.
fn ternary_type(db: &Db, def: DefId) -> Ty {
    let DefId::Function(id) = def else {
        panic!("expected a function")
    };
    let body = db.body_map(BodyOwnerId::Function(id));
    let inference = db.inference(def);
    body.syntax_store
        .exprs
        .iter()
        .find_map(|(expr, e)| matches!(e, Expr::Ternary { .. }).then(|| inference.expr_ty(expr)))
        .expect("the fixture should contain a ternary")
}

/// `c ? a : b` folds to the smallest type both branches reach: two literals
/// take their common width (`1`/`300` is `uint16`, not `uint8`), and a literal
/// branch widens to the typed branch.
#[test]
fn ternary_branches_fold_to_a_common_type() {
    for (branches, expected) in [
        ("c ? 1 : 2", Primitive::Uint(8)),
        ("c ? 1 : 300", Primitive::Uint(16)),
        ("c ? uint8(1) : uint256(2)", Primitive::Uint(256)),
    ] {
        let source =
            format!("contract C {{ function f(bool c) external pure {{ {branches}; }} }}");
        let (db, file) = make_test_db(&source);
        let inference = named_function_inference(&db, file, "f");
        assert!(
            inference.diagnostics.is_empty(),
            "{branches}: {:?}",
            codes(&inference)
        );
        let ty = ternary_type(&db, find_function(&db, file, "f"));
        assert!(
            matches!(ty.kind(&db), TyKind::Primitive(p) if p == expected),
            "`{branches}` should be {expected:?}, got {}",
            ty.display(&db)
        );
    }

    // A fractional literal has no concrete mobile type in this subset — solc
    // reaches a fixed-point type — so the fractional branch stands in instead
    // of the conditional being rejected as having no common type.
    let (db, file) = make_test_db(
        "contract C { function f(bool c) external pure { c ? 1.5 : 2.5; c ? 1 : 1.5; c ? 1.5 : 2; } }",
    );
    let inference = named_function_inference(&db, file, "f");
    assert!(
        !inference
            .diagnostics
            .iter()
            .any(|d| matches!(d, SemanticDiagnostic::TypeMismatch { .. })),
        "{:?}",
        codes(&inference),
    );
}

/// The condition must be boolean, exactly as `require`'s is, and the branches
/// must have a common type.
#[test]
fn ternary_rejects_a_non_boolean_condition_or_unrelated_branches() {
    for source in [
        "contract C { function f(uint256 c) external pure returns (uint256) { return c ? 1 : 2; } }",
        "contract C { function f(bool c) external pure { c ? true : 1; } }",
    ] {
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

/// A string literal converts to `bytesN` only when it *fits*, and the fit is
/// the decoded length: `"\x00\x00"` is two bytes, not eight, and `"\u00e9"` is
/// the two UTF-8 bytes of `é`.
#[test]
fn string_literal_escapes_fit_bytes_n_by_their_decoded_length() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function ok() external pure {
                    bytes1 a = "\n";
                    bytes2 b = "\x00\x00";
                    bytes2 c = "\u00e9";
                    bytes3 d = "abc";
                    bytes memory e = "\x00\x00";
                }
                function tooSmall() external pure {
                    bytes1 f = "ab";
                    bytes1 g = "\x00\x00";
                }
            }
            "#,
    );
    let ok = named_function_inference(&db, file, "ok");
    assert!(ok.diagnostics.is_empty(), "{:?}", codes(&ok));

    let too_small = named_function_inference(&db, file, "tooSmall");
    assert!(
        too_small
            .diagnostics
            .iter()
            .any(|d| matches!(d, SemanticDiagnostic::TypeMismatch { .. })),
        "{:?}",
        codes(&too_small),
    );
}

/// A conditional expression is not a place.
#[test]
fn ternary_is_not_an_lvalue() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function f(bool c) external pure {
                    uint256 a; uint256 b;
                    (c ? a : b) = 1;
                }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "f");
    assert!(
        inference.diagnostics.iter().any(|d| matches!(d,
            SemanticDiagnostic::InferenceFailure { message, .. }
                if message.contains("cannot be assigned to"))),
        "{:?}",
        codes(&inference),
    );
}

/// Every branch is lowered before the conditional is built. `_` lowers to no
/// expression, so the ternary itself cannot be allocated — but the false
/// branch's literal must still be in the store, not skipped by an early `?`.
#[test]
fn ternary_lowers_every_branch_before_failing() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function f(bool c) external pure returns (uint256) {
                    return c ? _ : 42;
                }
            }
            "#,
    );
    let DefId::Function(id) = find_function(&db, file, "f") else {
        panic!("fixture declares function f")
    };
    let body = db.body_map(BodyOwnerId::Function(id));
    assert!(
        !body
            .syntax_store
            .exprs
            .iter()
            .any(|(_, e)| matches!(e, Expr::Ternary { .. })),
        "the ternary should not be built when an arm fails to lower"
    );
    let false_branch = body.syntax_store.exprs.iter().any(|(_, e)| {
        matches!(e, Expr::Literal(crate::hir::exprs::Literal::Number(n)) if n.as_str() == "42")
    });
    assert!(
        false_branch,
        "the false branch should still be lowered past the failing arm"
    );
}

/// Unary and update expressions keep their operator in HIR and type from their
/// operand: `!` yields `bool`, `-` and `~` keep the operand's type, `++`/`--`
/// yield the operand's type, and `delete` yields nothing.
#[test]
fn unary_and_update_expressions_type_from_their_operand() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function f(uint256 x, bool c) external pure {
                    bool a = !c;
                    uint256 b = -x;
                    uint256 d = ~x;
                    uint256 e = x++;
                    uint256 g = ++x;
                    delete x;
                }
                function bad(uint256 x) external pure {
                    bool a = !x;
                }
            }
            "#,
    );
    let ok = named_function_inference(&db, file, "f");
    assert!(ok.diagnostics.is_empty(), "{:?}", codes(&ok));

    let DefId::Function(id) = find_function(&db, file, "f") else {
        panic!("fixture declares function f")
    };
    let body = db.body_map(BodyOwnerId::Function(id));
    let operators = body
        .syntax_store
        .exprs
        .iter()
        .filter_map(|(_, expr)| match expr {
            Expr::Unary { op, .. } => Some(op.as_str()),
            Expr::Update { op, .. } => Some(op.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(operators, ["!", "-", "~", "++", "++", "delete"]);

    // `!x` on an unsigned is not a `bool`, so the conversion is refused.
    let bad = named_function_inference(&db, file, "bad");
    assert!(
        bad.diagnostics
            .iter()
            .any(|d| matches!(d, SemanticDiagnostic::TypeMismatch { .. })),
        "{:?}",
        codes(&bad)
    );
}

// ── Comments inside expressions ─────────────────────────────────────────
// `comment` is a *named* child and interleaves inside operand positions —
// `a && // note\n b` parses `binary_expression` as `[expr, comment, expr]`.
// Positional operand reads skip comments, or the operand behind one is lost.

/// Every operand position that must skip an interleaved comment. The binary
/// case is Strings.sol's SPECIAL_CHARS_LOOKUP (`(1 << 0x08) | // note\n …`);
/// the return cases are the RSA.sol / WebAuthn.sol pattern.
#[test]
fn comments_do_not_break_positional_lowering() {
    for source in [
        // between binary operands
        "contract C { function f() external returns (uint) { return (1 << 0x08) | // note\n (1 << 0x09); } }",
        "contract C { uint256 constant X = (1 << 0x08) | // note\n (1 << 0x09); }",
        // between ternary operands
        "contract C { function f(bool c) external returns (uint) { return c ? /* note */ 1 : 2; } }",
        // before a unary operand
        "contract C { function f(bool x) external pure returns (bool) { return !/* note */x; } }",
        // inside `call_argument` — the comment is not the argument
        "contract C { function g(uint x) internal pure {} function f() external { g(/* note */1); } }",
        // inside a tuple — not a component, not a hole
        "contract C { function f(uint a, uint b) external { (a /* note */, b); } }",
        // inside a single-child wrapper — the bubble still happens
        "contract C { function f(bool a, bool b) external returns (bool) { return (/* note */ a && b); } }",
        // inside an inline array — not an element
        "contract C { function f() external pure { uint8[3] memory x = [1, /* note */ 2, 3]; } }",
        // inside a return value — not the value, not dropped into `Return(None)`
        "contract C { function f(bool a, bool b) external returns (bool) { return a && // note\n b; } }",
        "contract C { function f(uint a) external returns (uint) { return /* note */ a; } }",
    ] {
        let (db, file) = make_test_db(source);
        let diagnostics: Vec<_> = db
            .file_def_map(file)
            .defs
            .keys()
            .flat_map(|def| {
                db.inference(*def)
                    .diagnostics
                    .iter()
                    .map(SemanticDiagnostic::code)
                    .collect::<Vec<_>>()
            })
            .collect();
        assert!(diagnostics.is_empty(), "{source}: {diagnostics:?}");
    }
}

// ── Slices ────────────────────────────────────────────────────────────

/// `a[from:to]` — every slice form lowers and types without diagnostics on
/// the sliceable bases (`bytes`, `string`, `T[]`, all `calldata`), and the
/// `Slice` type converts through its base: a `bytes calldata` slice reaches
/// `bytes calldata` parameters and `bytes memory` variables alike.
#[test]
fn calldata_slices_type_and_convert_through_the_base() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function inner(bytes calldata b) internal pure returns (uint256) { return b.length; }
                function f(bytes calldata b, string calldata s, uint256[] calldata a) external pure {
                    bytes calldata x = b[1:2];
                    bytes calldata y = b[1:];
                    bytes calldata z = b[:3];
                    bytes calldata w = b[:];
                    string calldata t = s[0:5];
                    uint256[] calldata u = a[2:4];
                    bytes calldata nested = b[0:4][1:2];
                    bytes memory q = b[0:1];
                    inner(b[0:2]);
                }
            }
            "#,
    );
    let f = named_function_inference(&db, file, "f");
    assert!(f.diagnostics.is_empty(), "{:?}", codes(&f));
    assert!(
        expr_results(&f).any(|(_, ty, _)| matches!(ty.kind(&db), TyKind::Slice(_))),
        "a slice should type as TyKind::Slice"
    );
}

/// A slice is its own type, not the array: it has no members (`b[1:2].length`
/// is unresolved, exactly like solc), it is never a place, and its bounds are
/// `uint256` values.
#[test]
fn a_slice_has_no_members_and_is_no_place() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function members(bytes calldata b) external pure {
                    uint l = b[1:2].length;
                }
                function bad_bound(bytes calldata b, bool flag) external pure {
                    bytes calldata x = b[flag:2];
                }
                function bad_place(bytes calldata b) external pure {
                    b[0:1] = b[0:1];
                }
            }
            "#,
    );
    let members = named_function_inference(&db, file, "members");
    assert!(
        members
            .diagnostics
            .iter()
            .any(|d| matches!(d, SemanticDiagnostic::UnresolvedMember { .. })),
        "{:?}",
        codes(&members),
    );
    let bad_bound = named_function_inference(&db, file, "bad_bound");
    assert!(
        bad_bound
            .diagnostics
            .iter()
            .any(|d| matches!(d, SemanticDiagnostic::TypeMismatch { .. })),
        "{:?}",
        codes(&bad_bound),
    );
    let bad_place = named_function_inference(&db, file, "bad_place");
    assert!(
        bad_place.diagnostics.iter().any(|d| matches!(d,
            SemanticDiagnostic::InferenceFailure { message, .. }
                if message.contains("cannot be assigned to"))),
        "{:?}",
        codes(&bad_place),
    );
}

/// Memory, storage, fixed-size and non-array bases all reject — solc's two
/// messages: "only supported for dynamic calldata arrays" for array-ish
/// bases at the wrong shape, "only possible for arrays and array slices"
/// for non-arrays.
#[test]
fn only_dynamic_calldata_arrays_slice() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                uint256[] store;
                function mem(bytes memory m) external pure {
                    bytes memory q = m[0:1];
                }
                function fixed(uint256[3] calldata a) external pure {
                    uint256[3] calldata x = a[0:2];
                }
                function non_array(bytes32 b) external pure {
                    bytes1 y = b[0:1];
                }
                function dynamic_base(bytes[] calldata a) external pure {
                    bytes[] calldata x = a[0:1];
                }
            }
            "#,
    );
    for name in ["mem", "fixed"] {
        let inference = named_function_inference(&db, file, name);
        assert!(
            inference.diagnostics.iter().any(|d| matches!(d,
                SemanticDiagnostic::InferenceFailure { message, .. }
                    if message.contains("only supported for dynamic calldata arrays"))),
            "{name}: {:?}",
            codes(&inference),
        );
    }
    let non_array = named_function_inference(&db, file, "non_array");
    assert!(
        non_array.diagnostics.iter().any(|d| matches!(d,
            SemanticDiagnostic::InferenceFailure { message, .. }
                if message.contains("only possible for arrays and array slices"))),
        "{:?}",
        codes(&non_array),
    );
    let dynamic_base = named_function_inference(&db, file, "dynamic_base");
    assert!(
        dynamic_base.diagnostics.iter().any(|d| matches!(d,
            SemanticDiagnostic::InferenceFailure { message, .. }
                if message.contains("dynamically encoded base types"))),
        "{:?}",
        codes(&dynamic_base),
    );

    // A bad base still produces the slice type, so the solc cascade follows:
    // `bytes memory q = m[0:1]` reports the access *and* "bytes memory slice
    // is not implicitly convertible to expected type bytes memory".
    let mem = named_function_inference(&db, file, "mem");
    assert!(
        mem.diagnostics
            .iter()
            .any(|d| matches!(d, SemanticDiagnostic::TypeMismatch { .. })),
        "{:?}",
        codes(&mem),
    );
}

// ── Inline arrays ─────────────────────────────────────────────────────

/// `[1, 2, 3]` is a fresh `uint8[3] memory` — typed by the elements' common
/// mobile type, usable as an indexable, length-bearing value.
#[test]
fn inline_arrays_type_from_their_elements() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function g(uint8[3] memory a) internal pure {}
                function f() external pure returns (uint256) {
                    uint8[3] memory x = [1, 2, 3];
                    uint16[2] memory y = [1, 300];
                    uint8[2][2] memory n = [[1, 2], [3, 4]];
                    uint256 l = [1, 2, 3].length;
                    uint8 e = [1, 2, 3][0];
                    g([1, 2, 3]);
                    x[0] = 5;
                    [1, 2, 3][0] = 5;
                    return l + e;
                }
            }
            "#,
    );
    let f = named_function_inference(&db, file, "f");
    assert!(f.diagnostics.is_empty(), "{:?}", codes(&f));
    let literal_ty = expr_results(&f).find_map(|(_, ty, _)| {
        let TyKind::Ref(inner, Location::Memory) = ty.kind(&db) else {
            return None;
        };
        matches!(
            inner.kind(&db),
            TyKind::Array { size: Some(_), .. }
        )
        .then_some(ty)
    });
    assert!(
        literal_ty.is_some(),
        "expected an inline array to type as `T[k] memory`"
    );
}

/// `[]`, mixed kinds, and a deduced-but-unnameable common type all reject.
/// `[uint8(1), uint16(2)]` deduces `uint16[2] memory` — the *expected* type
/// plays no part — so the `uint8[2]` target reports a mismatch.
#[test]
fn inline_array_element_types_must_agree() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function empty() external pure { uint8[] memory a = []; }
                function mixed() external pure { uint8[2] memory a = [1, true]; }
                function signed_mismatch() external pure { uint8[2] memory a = [1, -1]; }
                function widened() external pure { uint8[2] memory a = [uint8(1), uint16(2)]; }
            }
            "#,
    );
    for name in ["empty", "mixed", "signed_mismatch"] {
        let inference = named_function_inference(&db, file, name);
        assert!(
            inference.diagnostics.iter().any(|d| matches!(d,
                SemanticDiagnostic::InferenceFailure { message, .. }
                    if message.contains("Unable to deduce"))),
            "{name}: {:?}",
            codes(&inference),
        );
    }
    let widened = named_function_inference(&db, file, "widened");
    assert!(
        widened
            .diagnostics
            .iter()
            .any(|d| matches!(d, SemanticDiagnostic::TypeMismatch { .. })),
        "{:?}",
        codes(&widened),
    );
}

// ── State effects ─────────────────────────────────────────────────────
// A body accumulates one effect (`Pure` < `View` < `NonPayable`) from what it
// reads, writes, and calls, and `require_effect` compares it against the
// declaration. These tests pin the classification of each kind of site: a
// state *write* is not also a read, a write to memory is not a state write, and
// a modifier's effect reaches the function that invokes it.


/// The effect levels an owner's body could not afford, in diagnostic order.
fn required(inference: &Inference) -> Vec<Mutability> {
    inference
        .diagnostics
        .iter()
        .filter_map(|d| match d {
            SemanticDiagnostic::StateMutability { required, .. } => Some(*required),
            _ => None,
        })
        .collect()
}

fn assert_requires(source: &str, function: &str, expected: &[Mutability]) {
    let (db, file) = make_test_db(source);
    let inference = named_function_inference(&db, file, function);
    assert_eq!(
        required(&inference),
        expected,
        "{function}: {:?}",
        codes(&inference)
    );
}

/// Every shape of state write: a bare variable, a member, an index, and a
/// tuple component. `view` and `pure` both refuse them, and the write is
/// reported as a write — not as the read of the place it targets.
#[test]
fn state_writes_are_rejected_in_view_and_pure() {
    let source = r#"
        struct S { uint x; }
        contract C {
            uint state;
            uint[] arr;
            S s;
            function assignView() public view { state = 1; }
            function assignPure() public pure { state = 1; }
            function memberView() public view { s.x = 1; }
            function memberPure() public pure { s.x = 1; }
            function indexView() public view { arr[0] = 1; }
            function indexPure() public pure { arr[0] = 1; }
            function tupleView() public view { uint local; (state, local) = (1, 2); }
            function tuplePure() public pure { uint local; (state, local) = (1, 2); }
            function writes() public { state = 1; }
        }
    "#;
    for name in [
        "assignView",
        "assignPure",
        "memberView",
        "memberPure",
        "indexView",
        "indexPure",
        "tupleView",
        "tuplePure",
    ] {
        assert_requires(source, name, &[Mutability::NonPayable]);
    }
    assert_requires(source, "writes", &[]);
}

/// Writing memory is not a state write, but reading a state variable for the
/// index still is: `memArr[state] = 1` in `pure` reports only the read. The
/// common `memory` writes — an array element and a struct field — stay legal in
/// `pure`, which is what keeps the write check from misfiring.
#[test]
fn a_memory_write_still_reads_its_index() {
    assert_requires(
        r#"
        contract C {
            uint state;
            function f() public pure { uint[] memory memArr = new uint[](2); memArr[state] = 1; }
        }
        "#,
        "f",
        &[Mutability::View],
    );
    let memory = r#"
        struct S { uint x; }
        contract C {
            function array() public pure { uint[] memory memArr = new uint[](2); memArr[0] = 1; }
            function field() public pure { S memory s; s.x = 1; }
        }
    "#;
    assert_requires(memory, "array", &[]);
    assert_requires(memory, "field", &[]);
}

/// A `storage` pointer is a place in storage, so writing through it is a state
/// write and reading through it is a state read.
#[test]
fn writes_through_a_storage_pointer_are_state_writes() {
    let source = r#"
        contract C {
            uint[] stateArr;
            function paramView(uint[] storage p) internal view { p[0] = 1; }
            function paramPure(uint[] storage p) internal pure { p[0] = 1; }
            function localView() internal view { uint[] storage p = stateArr; p[0] = 1; }
            function readsOk(uint[] storage p) internal view returns (uint) { return p[0]; }
            function initPure() internal pure { uint[] storage p = stateArr; p[0] = 1; }
            function writesOk(uint[] storage p) internal { p[0] = 1; }
        }
    "#;
    for name in ["paramView", "paramPure", "localView"] {
        assert_requires(source, name, &[Mutability::NonPayable]);
    }
    // Reading a pointer is a read, and reading state to initialize one is too.
    assert_requires(source, "readsOk", &[]);
    assert_requires(source, "initPure", &[Mutability::View, Mutability::NonPayable]);
    assert_requires(source, "writesOk", &[]);
}

/// The effect a modifier's body needs is required of the function that applies
/// it, reported at the invocation.
#[test]
fn modifier_effects_reach_the_invoking_function() {
    let source = r#"
        contract C {
            uint state;
            modifier touchesState() { state = 1; _; }
            modifier readsEnv() { require(msg.sender != address(0)); _; }
            function viewWrites() public view touchesState {}
            function pureWrites() public pure touchesState {}
            function viewReads() public view readsEnv {}
            function pureReads() public pure readsEnv {}
            function ok() public touchesState {}
        }
    "#;
    assert_requires(source, "viewWrites", &[Mutability::NonPayable]);
    assert_requires(source, "pureWrites", &[Mutability::NonPayable]);
    assert_requires(source, "pureReads", &[Mutability::View]);
    assert_requires(source, "viewReads", &[]);
    assert_requires(source, "ok", &[]);
}

/// A function-pointer call has no declaration to select, but its signature
/// still says what it may do. A pointer held in a parameter is the clean case;
/// a pointer held in state is also a state *read*, which is why even a `pure`
/// pointer stored in state needs `view` to be called.
#[test]
fn function_pointer_calls_carry_their_mutability() {
    let source = r#"
        contract C {
            function() external pure statePureFp;
            function callView(function() external f) internal view { f(); }
            function callViewOk(function() external view f) internal view { f(); }
            function callPureBad(function() external view f) internal pure { f(); }
            function callPureOk(function() external pure f) internal pure { f(); }
            function callOk(function() external f) internal { f(); }
            function statePureFpBad() public pure { statePureFp(); }
        }
    "#;
    assert_requires(source, "callView", &[Mutability::NonPayable]);
    assert_requires(source, "callViewOk", &[]);
    assert_requires(source, "callPureBad", &[Mutability::View]);
    assert_requires(source, "callPureOk", &[]);
    assert_requires(source, "callOk", &[]);
    assert_requires(source, "statePureFpBad", &[Mutability::View]);
}

/// `delete` and `++`/`--` write their operand, so they count as state writes
/// exactly when the operand lives in storage.
#[test]
fn delete_and_update_write_their_operand() {
    let source = r#"
        contract C {
            uint state;
            uint[] arr;
            function deleteView() public view { delete state; }
            function deleteIndexPure() public pure { delete arr[0]; }
            function updateView() public view { state++; }
            function updateOk() public { ++state; }
            function localPure() public pure { uint x; x = 1; ++x; delete x; }
        }
    "#;
    assert_requires(source, "deleteView", &[Mutability::NonPayable]);
    assert_requires(source, "deleteIndexPure", &[Mutability::NonPayable]);
    assert_requires(source, "updateView", &[Mutability::NonPayable]);
    assert_requires(source, "updateOk", &[]);
    assert_requires(source, "localPure", &[]);
}

/// A body needs the strictest thing it does: a read then a write needs
/// `non-payable`, a read alone needs `view`, and a pure call needs nothing.
#[test]
fn a_body_needs_the_strictest_effect_it_uses() {
    let source = r#"
        contract C {
            uint state;
            function pureFn() public pure returns (uint) { return 1; }
            function readOnly() public pure returns (uint) { return state; }
            function readThenWrite() public view { uint x = state; state = x; }
            function readsThenCalls() public pure { pureFn(); }
        }
    "#;
    assert_requires(source, "readOnly", &[Mutability::View]);
    assert_requires(source, "readThenWrite", &[Mutability::NonPayable]);
    assert_requires(source, "readsThenCalls", &[]);
}

/// The accumulated effect is what a caller sees, not the declaration: a `view`
/// function that only reads requires `view`, a non-payable one that writes
/// requires `nonpayable`, and a modifier records the effect its body needs.
#[test]
fn the_accumulated_effect_is_recorded_on_the_tables() {
    let (db, file) = make_test_db(
        r#"
        contract C {
            uint state;
            function reading() public view returns (uint) { return state; }
            function writing() public { state = 1; }
            function pureFn() public pure returns (uint) { return 1; }
            modifier touchesState() { state = 1; _; }
            modifier readsEnv() { require(msg.sender != address(0)); _; }
            modifier plain() { _; }
        }
        "#,
    );
    for (name, effect) in [
        ("reading", Mutability::View),
        ("writing", Mutability::NonPayable),
        ("pureFn", Mutability::Pure),
    ] {
        let inference = named_function_inference(&db, file, name);
        assert_eq!(inference.effect, effect, "{name}: {:?}", codes(&inference));
    }
    for (name, effect) in [
        ("touchesState", Mutability::NonPayable),
        ("readsEnv", Mutability::View),
        ("plain", Mutability::Pure),
    ] {
        let inference = named_modifier_inference(&db, file, name);
        assert_eq!(inference.effect, effect, "{name}: {:?}", codes(&inference));
        // A modifier declares no mutability of its own — the check belongs to
        // the function that applies it — so its own inference stays clean.
        assert!(inference.diagnostics.is_empty(), "{name}: {:?}", codes(&inference));
    }
}

/// A storage-pointer *parameter* is a pointer the caller handed in: solc does
/// not count its reads, while a body-declared pointer's reads are still state
/// reads. (OZ's `string storage store` helpers in `pure` rely on this.)
#[test]
fn reads_through_a_storage_parameter_are_not_state_reads() {
    let source = r#"
        contract C {
            uint[] stateArr;
            function paramRead(uint[] storage p) internal pure returns (uint) { return p[0]; }
            function paramReadStr(string storage s) internal pure returns (string memory) { return s; }
            function localRead() internal pure returns (uint) { uint[] storage p = stateArr; return p[0]; }
        }
    "#;
    assert_requires(source, "paramRead", &[]);
    assert_requires(source, "paramReadStr", &[]);
    // A body-declared pointer aliases state, so its read is a state read.
    assert_requires(source, "localRead", &[Mutability::View, Mutability::View]);
}

/// Sites solc classifies as pure: UDVT `wrap`/`unwrap` (OZ's TransientSlot
/// and ShortStrings rely on it throughout `pure`/`view` helpers), `immutable`
/// and `constant` reads (they live in the deployed code, not storage),
/// `assert`/`require` (declared `pure`), and — on the other side — `now`,
/// which aliases `block.timestamp` and needs `view`.
#[test]
fn pure_sites_require_nothing() {
    assert_requires(
        r#"
        type T is bytes32;
        contract C {
            function wrapIt(bytes32 b) internal pure returns (T) { return T.wrap(b); }
            function unwrapIt(T t) internal pure returns (bytes32) { return T.unwrap(t); }
        }
        "#,
        "wrapIt",
        &[],
    );
    assert_requires(
        r#"
        type T is bytes32;
        contract C {
            function unwrapIt(T t) internal pure returns (bytes32) { return T.unwrap(t); }
        }
        "#,
        "unwrapIt",
        &[],
    );
    let source = r#"
        contract C {
            uint immutable x;
            uint constant y = 1;
            uint s;
            function readImmutable() public pure returns (uint) { return x; }
            function readConstant() public pure returns (uint) { return y; }
            function readState() public pure returns (uint) { return s; }
            function assertIt() public pure { assert(true); }
            function requireIt() public pure { require(true, "x"); }
            function nowView() public view returns (uint) { return now(); }
            function nowPure() public pure returns (uint) { return now(); }
        }
    "#;
    for name in [
        "readImmutable",
        "readConstant",
        "assertIt",
        "requireIt",
        "nowView",
    ] {
        assert_requires(source, name, &[]);
    }
    for (name, effect) in [("readState", Mutability::View), ("nowPure", Mutability::View)] {
        assert_requires(source, name, &[effect]);
    }
}

/// `a.balance`/`.code`/`.codehash` probe the *account* — an environment read
/// (`EXTBALANCE`/`EXTCODE*`) regardless of where `a` lives, so `pure` may not
/// read them while `view` may.
#[test]
fn account_probes_read_the_environment() {
    let source = r#"
        contract C {
            function balancePure(address a) public pure returns (uint) { return a.balance; }
            function codePure(address a) public pure returns (bytes memory) { return a.code; }
            function codehashPure(address a) public pure returns (bytes32) { return a.codehash; }
            function balanceView(address a) public view returns (uint) { return a.balance; }
            function selectorPure() public pure returns (bytes4) { return this.selectorPure.selector; }
            function bareThis() public pure returns (address) { return address(this); }
            function thisBalance() public pure returns (uint) { return address(this).balance; }
        }
    "#;
    for name in ["balancePure", "codePure", "codehashPure"] {
        assert_requires(source, name, &[Mutability::View]);
    }
    // `this`/`address(this)` itself is pure-legal — solc counts the env
    // access where it happens: the getter call, the `.balance` probe.
    assert_requires(source, "balanceView", &[]);
    assert_requires(source, "selectorPure", &[]);
    assert_requires(source, "bareThis", &[]);
    assert_requires(source, "thisBalance", &[Mutability::View]);
}

/// `return e;` lowers to an `Expr::Return` whose value is a child expression,
/// so the returned value is checked as a value of the enclosing function.
#[test]
fn return_statement_lowers_to_a_return_node_with_its_value() {
    let (db, file) =
        make_test_db("contract C { function f() external pure returns (uint) { return 1; } }");
    let DefId::Function(id) = find_function(&db, file, "f") else {
        panic!("fixture declares function f")
    };
    let body = db.body_map(BodyOwnerId::Function(id));
    let (return_id, value) = body
        .syntax_store
        .exprs
        .iter()
        .find_map(|(id, expr)| match expr {
            Expr::Return(value) => Some((id, *value)),
            _ => None,
        })
        .expect("the fixture has a return statement");
    let value = value.expect("`return 1` carries its value");
    // Children are allocated before their parent, so the value precedes it.
    assert!(value < return_id);
    // A `return` produces no value of its own.
    assert!(matches!(
        db.inference(DefId::Function(id))
            .expr_ty(return_id)
            .kind(&db),
        TyKind::Void
    ));
}

/// The returned value is checked against the signature — top-level or nested
/// in a block — and arity is checked too: a value in a valueless function and
/// a bare `return` where returns exist are both "different number of
/// arguments". A modifier has no return parameters, so it can stop with a bare
/// `return` but can never return a value.
#[test]
fn returned_values_are_checked_against_the_signature() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function ok() external pure returns (uint) { return 1; }
                function bad() external pure returns (uint) { return true; }
                function nested() external pure returns (uint) { if (true) { return true; } return 1; }
                function valueless() external pure { return 1; }
                function empty() external pure returns (uint) { return; }
                function tuple_ok() external pure returns (uint, address) { return (1, address(0)); }
                function arity() external pure returns (uint, address) { return 1; }
                function too_many() external pure returns (uint) { return (1, 2); }
                function element() external pure returns (uint, address) { return (1, true); }
            }
            "#,
    );
    for name in ["ok", "tuple_ok"] {
        let inference = named_function_inference(&db, file, name);
        assert!(
            inference.diagnostics.is_empty(),
            "{name}: {:?}",
            codes(&inference)
        );
    }
    for name in ["bad", "nested", "element"] {
        let inference = named_function_inference(&db, file, name);
        assert!(
            inference
                .diagnostics
                .iter()
                .any(|d| matches!(d, SemanticDiagnostic::TypeMismatch { .. })),
            "{name}: {:?}",
            codes(&inference),
        );
    }
    for name in ["valueless", "arity", "too_many"] {
        let inference = named_function_inference(&db, file, name);
        assert!(
            inference.diagnostics.iter().any(|d| matches!(d,
                SemanticDiagnostic::InferenceFailure { message, .. }
                    if message.contains("different number of arguments"))),
            "{name}: {:?}",
            codes(&inference),
        );
    }
    let empty = named_function_inference(&db, file, "empty");
    assert!(
        empty.diagnostics.iter().any(|d| matches!(d,
            SemanticDiagnostic::InferenceFailure { message, .. }
                if message.contains("return arguments required"))),
        "{:?}",
        codes(&empty),
    );

    let (db, file) = make_test_db(
        r#"
            contract C {
                modifier ok() { return; _; }
                modifier bad() { return 1; _; }
            }
            "#,
    );
    let ok = db.inference(find_modifier(&db, file, "ok"));
    assert!(ok.diagnostics.is_empty(), "{:?}", codes(&ok));
    let bad = db.inference(find_modifier(&db, file, "bad"));
    assert!(
        bad.diagnostics.iter().any(|d| matches!(d,
            SemanticDiagnostic::InferenceFailure { message, .. }
                if message.contains("return arguments not allowed"))),
        "{:?}",
        codes(&bad),
    );
}

/// Unresolved pieces of a return report once, without cascading: an
/// unresolved value is a single diagnostic, and an unresolved return type
/// leaves the signature unavailable so no arity error piles on.
#[test]
fn unresolved_return_parts_do_not_cascade() {
    let (db, file) = make_test_db(
        "contract C { function f() external pure returns (uint) { return missing; } }",
    );
    let f = named_function_inference(&db, file, "f");
    assert_eq!(
        f.diagnostics
            .iter()
            .filter(|d| matches!(d, SemanticDiagnostic::UnresolvedName { .. }))
            .count(),
        1,
        "{:?}",
        codes(&f)
    );
    assert_eq!(f.diagnostics.len(), 1, "{:?}", codes(&f));

    let (db, file) =
        make_test_db("contract C { function f() external pure returns (Missing) { return 1; } }");
    let f = named_function_inference(&db, file, "f");
    assert!(
        f.diagnostics
            .iter()
            .any(|d| matches!(d, SemanticDiagnostic::UnresolvedType { .. })),
        "{:?}",
        codes(&f),
    );
    assert!(
        !f.diagnostics.iter().any(|d| matches!(d,
            SemanticDiagnostic::InferenceFailure { message, .. }
                if message.contains("different number of arguments"))),
        "{:?}",
        codes(&f),
    );
}

/// A returned value is a value first: a bare overload set has no value to
/// return, so the `Rvalue` rule fires and the conversion check (which would
/// see only `Invalid`) stays quiet (solc: "No matching declaration found
/// after variable lookup").
#[test]
fn a_bare_overload_set_cannot_be_returned() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function f(uint x) internal pure returns (uint) { return x; }
                function f(bool b) internal pure returns (uint) { return b ? 1 : 0; }
                function g() external pure returns (uint) { return f; }
            }
            "#,
    );
    let g = named_function_inference(&db, file, "g");
    assert!(
        g.diagnostics.iter().any(|d| matches!(d,
            SemanticDiagnostic::InferenceFailure { message, .. }
                if message.contains("no matching declaration"))),
        "{:?}",
        codes(&g),
    );
    assert_eq!(g.diagnostics.len(), 1, "{:?}", codes(&g));
}
