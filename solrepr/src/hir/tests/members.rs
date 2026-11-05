use super::*;

#[test]
fn struct_field_access_resolves_field_type() {
    let (db, file) = make_test_db(
        r#"
            struct S { uint x; }
            contract C {
                function f() external pure returns (uint) {
                    S memory s;
                    return s.x;
                }
            }
            "#,
    );
    let inference = function_inference(&db, file);
    let field_result = expr_results(&inference).any(|(_, ty, resolution)| {
        matches!(resolution, Some(Resolution::Field(_)))
            && matches!(ty.kind(&db), TyKind::Primitive(Primitive::Uint(256)))
    });
    assert!(
        field_result,
        "expected s.x to resolve to the uint field type"
    );
}

#[test]
fn library_function_is_accessible_through_types() {
    let (db, file) = make_test_db(
        r#"
            library L {
                function add(uint a, uint b) external pure returns (uint) { return a + b; }
            }
            contract C {
                function f() external pure returns (uint) { return L.add(1, 2); }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "f");
    let call_result = expr_results(&inference).any(|(_, ty, resolution)| {
        matches!(resolution, Some(Resolution::Callable(_)))
            && matches!(ty.kind(&db), TyKind::Primitive(Primitive::Uint(256)))
    });
    assert!(
        call_result,
        "expected L.add(1, 2) to resolve and type as uint"
    );
}

#[test]
fn getters_match_the_documented_complex_example() {
    // Verbatim from the Solidity docs, "Getter Functions": the mapping and
    // arrays (except byte arrays) in the struct are omitted, and it
    // "generates a function of the following form":
    //
    //   function data(uint arg1, bool arg2, uint arg3)
    //       public returns (uint a, bytes3 b, bytes memory e)
    let (db, file) = make_test_db(
        r#"
            contract Complex {
                struct Data {
                    uint a;
                    bytes3 b;
                    mapping(uint => uint) map;
                    uint[3] c;
                    uint[] d;
                    bytes e;
                }
                mapping(uint => mapping(bool => Data[])) public data;
                function f() external view {
                    this.data(1, true, 2);
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

    let getter_signatures: Vec<String> = expr_results(&inference)
        .filter_map(|(_, _, resolution)| {
            let Some(Resolution::Callable(callable)) = resolution else {
                return None;
            };
            if !callable.is_getter() {
                return None;
            }
            let DefId::Var(id) = callable.def()? else {
                return None;
            };
            db.getter_signature(id).map(|fnty| fnty.display(&db))
        })
        .collect();
    assert!(
        getter_signatures
            .iter()
            .any(|sig| sig == "function(uint256, bool, uint256) external view returns (uint256, bytes3, bytes memory)"),
        "unexpected getter signature: {getter_signatures:?}"
    );
}

/// Beyond the docs example: a nested mapping's getter unwraps one parameter
/// per key recursively; `this.x()` resolves to the getter and returns the
/// ABI copy (`string memory`), while a bare `x` is the storage reference.
#[test]
fn getters_unwrap_copy_and_resolve_like_external_functions() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                mapping(uint256 => mapping(address => bool)) public nested;
                uint public x;
                string public storageString;
                function f() external view returns (string memory) {
                    this.nested(1, address(0));
                    this.x();
                    storageString;
                    return this.storageString();
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
    assert!(
        expr_results(&inference).any(|(_, _, resolution)| {
            let Some(Resolution::Callable(callable)) = resolution else {
                return false;
            };
            if !callable.is_getter() {
                return false;
            }
            let Some(DefId::Var(id)) = callable.def() else {
                return false;
            };
            db.getter_signature(id).is_some_and(|fnty| {
                fnty.display(&db) == "function(uint256, address) external view returns (bool)"
            })
        }),
        "nested mappings should unwrap to one parameter per key"
    );
    // The getter is an external function whose returns are ABI values —
    // a copy, never a storage reference.
    assert!(
        expr_results(&inference).any(|(_, ty, resolution)| {
            matches!(resolution, Some(Resolution::Callable(c)) if c.is_getter())
                && matches!(ty.kind(&db), TyKind::Ref(_, Location::Memory))
        }),
        "`this.storageString()` should be a `string memory` copy"
    );
    assert!(
        expr_results(&inference)
            .any(|(_, ty, _)| { matches!(ty.kind(&db), TyKind::Ref(_, Location::Storage(_))) }),
        "a bare `storageString` reference should stay `string storage`"
    );
    assert!(expr_results(&inference).any(|(_, ty, resolution)| {
        matches!(resolution, Some(Resolution::Callable(callable)) if callable.is_getter())
            && matches!(ty.kind(&db), TyKind::Primitive(Primitive::Uint(256)))
    }), "expected this.x() to resolve to the public getter");
}

#[test]
fn state_reference_member_inherits_storage_location() {
    let (db, file) = make_test_db(
        r#"
            struct S { uint[] v; }
            contract C {
                S s;
                function f() external view returns (uint) { return s.v.length; }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "f");
    let member = expr_results(&inference).any(|(_, ty, resolution)| {
        matches!(resolution, Some(Resolution::Field(_)))
            && matches!(ty.kind(&db), TyKind::Ref(inner, Location::Storage(_))
                if matches!(inner.kind(&db), TyKind::Array { .. }))
    });
    assert!(
        member,
        "expected s.v to inherit the storage location of the state variable"
    );
}

/// Errors and events carry `.selector` (an `anonymous` event has none), and
/// reached through a type name they keep the *access path* tag — `C.E`,
/// `L.E`, `C.Ev`, `L.Ev` are all `ViaType` — and stay invocable through any
/// namespace (`revert C.E()` / `emit L.Ev()`). Verified against solc 0.8.37.
#[test]
fn error_and_event_members_follow_their_access_paths() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                error Bad(uint256);
                event Done(uint256 indexed);
                event Anon(uint256) anonymous;
                function f() external pure returns (bytes4, bytes32) {
                    return (Bad.selector, Done.selector);
                }
                function g() external pure returns (bytes32) { return Anon.selector; }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "f");
    assert!(
        !inference
            .diagnostics
            .iter()
            .any(|d| matches!(d, SemanticDiagnostic::UnresolvedMember { .. })),
        "unexpected: {:?}",
        codes(&inference)
    );
    let anon = named_function_inference(&db, file, "g");
    assert!(anon.diagnostics.iter().any(|d| {
        matches!(d, SemanticDiagnostic::UnresolvedMember { member, .. } if member == "selector")
    }));

    let cases = [
        (
            "contract C { error E(uint x); function f() external pure { revert C.E(1); } }",
            "a contract error via its contract name",
        ),
        (
            "library L { error E(uint x); } \
             contract C { function f() external pure { revert L.E(1); } }",
            "a library error via its library name",
        ),
        (
            "contract C { event Ev(uint x); function f() external { emit C.Ev(1); } }",
            "a contract event via its contract name",
        ),
        (
            "library L { event Ev(uint x); } \
             contract C { function f() external { emit L.Ev(1); } }",
            "a library event via its library name",
        ),
    ];
    for (source, label) in cases {
        let (db, file) = make_test_db(source);
        let inference = named_function_inference(&db, file, "f");
        assert!(
            inference.diagnostics.is_empty(),
            "{label}: {:?}",
            codes(&inference)
        );
        assert!(
            expr_results(&inference).any(|(_, _, resolution)| {
                matches!(resolution, Some(Resolution::Callable(c))
                    if matches!(c.kind(), CallKind::ViaType(_)) && c.is_invocable(&db))
            }),
            "{label}: the qualified reference should be a tagged, invocable callable",
        );
    }
}

/// `.selector` and `.address` are members of an *external* function — whether
/// the reference is `this.f`, `C.f`, or an internal library function's
/// pointer type. An internal function value has neither.
#[test]
fn function_members_are_external_only() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function f() external {}
                function g() external view {
                    bytes4 viaThis = this.f.selector;
                    address where = this.f.address;
                    bytes4 viaDecl = C.f.selector;
                }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "g");
    assert!(
        inference.diagnostics.is_empty(),
        "unexpected: {:?}",
        codes(&inference)
    );

    let (db, file) = make_test_db(
        r#"
            contract C {
                function f() internal {}
                function g() external view {
                    bytes4 s = f.selector;
                    address a = f.address;
                }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "g");
    for member in ["selector", "address"] {
        assert!(inference.diagnostics.iter().any(|d| {
            matches!(d, SemanticDiagnostic::UnresolvedMember { member: m, .. } if m == member)
        }));
    }
}

/// Each member table is gated by its host: `transfer` exists on
/// `address payable` only, `length` on `bytesNN` and arrays but never
/// `string`, and `push`/`pop` on a *storage* array only.
#[test]
fn member_tables_follow_their_hosts() {
    let cases: &[(&str, &str)] = &[
        (
            r#"
            contract C {
                function ok(address payable a) external { a.transfer(1); }
                function bad(address a) external { a.transfer(1); }
            }
            "#,
            "transfer",
        ),
        (
            r#"
            contract C {
                function ok(bytes4 x) external pure returns (uint8) { return x.length; }
                function bad(string memory s) external pure returns (uint256) { return s.length; }
            }
            "#,
            "length",
        ),
        (
            r#"
            contract C {
                uint256[] s;
                function ok() external { s.push(1); }
                function bad() external { uint256[] memory m; m.push(1); }
            }
            "#,
            "push",
        ),
    ];
    for (source, member) in cases {
        let (db, file) = make_test_db(source);
        let ok = named_function_inference(&db, file, "ok");
        assert!(!ok.diagnostics.iter().any(|d| {
            matches!(d, SemanticDiagnostic::UnresolvedMember { member: m, .. } if m == member)
        }));
        let bad = named_function_inference(&db, file, "bad");
        assert!(bad.diagnostics.iter().any(|d| {
            matches!(d, SemanticDiagnostic::UnresolvedMember { member: m, .. } if m == member)
        }));
    }
}

#[test]
fn string_concat_resolves_through_types() {
    let (db, file) =
        make_test_db(r#"contract C { function f() external { string.concat("a", "b"); } }"#);
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
}

#[test]
fn locals_shadow_builtins() {
    let (db, file) = make_test_db(
        r#"contract C { function f() external { uint256 msg = 1; uint256 y = msg; } }"#,
    );
    let inference = named_function_inference(&db, file, "f");
    let use_of_msg = expr_results(&inference)
        .any(|(_, _, resolution)| matches!(resolution, Some(Resolution::Local(_))));
    assert!(
        use_of_msg,
        "use of `msg` should resolve to the local, not the builtin"
    );
}

/// `super` resolves through the declaring contract's C3 tail — the next
/// *implemented* declaration — and sees only functions: a base state variable
/// is not a member.
#[test]
fn super_members_follow_the_declaring_c3() {
    let (db, file) = make_test_db(
        r#"
            contract A {
                function f() internal virtual returns (uint256) { return 1; }
            }
            contract B is A {
                function f() internal virtual override returns (uint256) { return 2; }
                uint value;
                function callMe() internal virtual {}
            }
            contract C is B {
                function g() external returns (uint256) { return super.f(); }
                function h() external { super.callMe(); }
                function bad() external { super.value; }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "g");
    assert!(inference.diagnostics.is_empty(), "{:?}", codes(&inference));
    let b = find_contract(&db, file, "B");
    assert!(expr_results(&inference).any(|(_, _, resolution)| {
        matches!(resolution, Some(Resolution::Callable(callable))
            if callable.def().is_some_and(|def| db.owner(def) == Some(b)))
    }));

    let h = named_function_inference(&db, file, "h");
    assert!(
        !h.diagnostics.iter().any(|d| {
            matches!(d, SemanticDiagnostic::UnresolvedMember { member, .. } if member == "callMe")
        }),
        "{:?}",
        codes(&h),
    );
    let bad = named_function_inference(&db, file, "bad");
    assert!(
        bad.diagnostics.iter().any(|d| {
            matches!(d, SemanticDiagnostic::UnresolvedMember { member, .. } if member == "value")
        }),
        "{:?}",
        codes(&bad),
    );
}

/// A `using`-attached function resolves on an instance receiver (`this`), on a
/// cast receiver, and when a sibling candidate is unavailable. The extension
/// is filtered by receiver convertibility — a first parameter that cannot
/// accept the receiver makes the member unavailable — and a same-named
/// declaration member collides into an ambiguity rather than shadowing.
#[test]
fn using_extensions_attach_to_convertible_receivers() {
    let cases = [
        (
            r#"
            library Extensions {
                function ext(C self) internal pure returns (uint256) {}
            }
            contract C {
                using Extensions for C;
                function f() external { this.ext(); }
            }
            "#,
            "ext",
        ),
        (
            r#"
            library Extensions {
                function ext(Missing self) internal pure returns (uint256) {}
                function ext(C self) internal pure returns (uint256) {}
            }
            contract C {
                using Extensions for C;
                function f() external { this.ext(); }
            }
            "#,
            "ext",
        ),
        (
            r#"
            interface IERC20 {
                function transfer(address to, uint256 amount) external returns (bool);
            }
            library SafeTransfer {
                function safeTransferFrom(
                    IERC20 token,
                    address from,
                    address to,
                    uint256 amount
                ) internal {}
            }
            contract C {
                using SafeTransfer for IERC20;
                function f(address token, address from, address to) external {
                    IERC20(token).safeTransferFrom(from, to, 1);
                }
            }
            "#,
            "safeTransferFrom",
        ),
    ];
    for (source, member) in cases {
        let (db, file) = make_test_db(source);
        let inference = named_function_inference(&db, file, "f");
        assert!(
            !inference.diagnostics.iter().any(|d| {
                matches!(d, SemanticDiagnostic::UnresolvedMember { member: m, .. } if m == member)
            }),
            "{source}: {:?}",
            codes(&inference),
        );
        assert!(
            expr_results(&inference).any(|(_, _, resolution)| {
                matches!(resolution, Some(Resolution::Callable(c)) if c.bound() == 1)
            }),
            "{source}: expected a using-attached callable",
        );
    }

    // A first parameter that cannot accept the receiver makes the member
    // unavailable.
    let (db, file) = make_test_db(
        r#"
            library Extensions {
                function ext(uint256 self) internal pure returns (uint256) {}
            }
            contract C {
                using Extensions for C;
                function f() external { this.ext(); }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "f");
    assert!(inference.diagnostics.iter().any(|d| {
        matches!(d, SemanticDiagnostic::UnresolvedMember { member, .. } if member == "ext")
    }));

    // A same-named declaration member collides into an ambiguity rather than
    // shadowing — for the call and the bare access alike.
    let (db, file) = make_test_db(
        r#"
            struct S { uint256 value; }
            library Extensions {
                function value(S memory self, uint256 amount) internal pure returns (uint256) {
                    return amount;
                }
            }
            contract C {
                using Extensions for S;
                function f(S memory s) external pure returns (uint256) {
                    return s.value(2);
                }
                function valueOnly(S memory s) external view returns (uint256) {
                    return s.value;
                }
            }
            "#,
    );
    for function in ["f", "valueOnly"] {
        let inference = named_function_inference(&db, file, function);
        assert!(
            inference.diagnostics.iter().any(|diagnostic| {
                matches!(diagnostic, SemanticDiagnostic::InferenceFailure { message, .. }
                    if message.contains("member access is ambiguous"))
            }),
            "{function}: {:?}",
            codes(&inference),
        );
        assert!(
            !expr_results(&inference).any(|(_, _, resolution)| {
                matches!(resolution, Some(Resolution::Callable(callable))
                    if callable.bound() == 1
                        && matches!(callable.def(), Some(DefId::Function(id))
                            if db.function_data(id).name == "value"))
            }),
            "ambiguous `value` must not resolve to its using function",
        );
    }
}

/// Members that do not exist on the receiver: a `constant`/`immutable` is not a
/// type-namespace member, `bytes.length` is a value member (the *type* has the
/// length), and a `using`-bound *free* function has no `.selector`.
#[test]
fn unavailable_members_are_reported() {
    let cases = [
        (
            r#"contract C { uint256 immutable I; function f() external { uint256 x = C.I; } }"#,
            "I",
        ),
        (
            r#"contract C { function f() external { bytes memory b = hex"00"; bytes.length; } }"#,
            "length",
        ),
        (
            r#"
            function inc(uint x) pure returns (uint) { return x + 1; }
            contract C {
                using {inc} for uint;
                function f() external pure returns (uint) {
                    uint x = 1;
                    bytes4 s = x.inc.selector;
                    return x.inc();
                }
            }
            "#,
            "selector",
        ),
    ];
    for (source, member) in cases {
        let (db, file) = make_test_db(source);
        let inference = named_function_inference(&db, file, "f");
        assert!(
            inference.diagnostics.iter().any(|d| {
                matches!(d, SemanticDiagnostic::UnresolvedMember { member: m, .. } if m == member)
            }),
            "{source}: {:?}",
            codes(&inference),
        );
    }
}

/// A library function reached through its type name (`L.add`) or attached to a
/// receiver (`x.inc`) is a callable value with a `.selector`.
#[test]
fn library_function_values_have_a_selector() {
    let cases = [
        (
            r#"
            library L {
                function add(uint a, uint b) external pure returns (uint) { return a + b; }
            }
            contract C {
                function f() external pure returns (uint) {
                    bytes4 s = L.add.selector;
                    return L.add(1, 2);
                }
            }
            "#,
            CallKind::ViaType(ViaTypeKind::Library),
        ),
        (
            r#"
            library L {
                function inc(uint self) public pure returns (uint) { return self + 1; }
            }
            contract C {
                using L for uint;
                function f() external pure returns (uint) {
                    uint x = 1;
                    bytes4 s = x.inc.selector;
                    return x.inc();
                }
            }
            "#,
            CallKind::Bound,
        ),
    ];
    for (source, kind) in cases {
        let (db, file) = make_test_db(source);
        let inference = named_function_inference(&db, file, "f");
        assert!(
            inference.diagnostics.is_empty(),
            "{source}: {:?}",
            codes(&inference)
        );
        assert!(
            expr_results(&inference).any(|(_, _, resolution)| {
                matches!(resolution, Some(Resolution::Callable(c))
                    if c.kind() == kind && c.is_invocable(&db))
            }),
            "{source}: expected a callable reference with the given kind",
        );
    }
}

/// A contract-qualified member resolves through the contract *type name* —
/// `C.K` a constant, `C.Bad` an error constructor.
#[test]
fn contract_qualified_members_resolve() {
    let fixtures = [
        r#"contract C { uint256 constant K = 1; function f() external { uint256 x = C.K; } }"#,
        r#"contract C { error Bad(); function f() external { revert C.Bad(); } }"#,
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

/// `Base.f` and `super.f` calls over the same fixture: internally-callable
/// base members are invocable on either path. On the explicit-base path an
/// `external` resolves but is a contract-type call ("declaration-only"),
/// while `private` is invisible entirely; on the `super` path both are
/// invisible — solc reports "Member not found". Verified against solc 0.8.33.
#[test]
fn base_and_super_calls_follow_the_access_matrix() {
    let declarations = r#"
            contract A {
                function inter() internal returns (uint) { return 1; }
                function pub_() public returns (uint) { return 2; }
                function ext() external returns (uint) { return 3; }
                function priv() private returns (uint) { return 4; }
            }
        "#;
    let bodies = |path: &str| {
        format!(
            "{declarations}
            contract C is A {{
                function ok_inter() external returns (uint) {{ return {path}.inter(); }}
                function ok_pub() external returns (uint) {{ return {path}.pub_(); }}
                function bad_ext() external returns (uint) {{ return {path}.ext(); }}
                function bad_priv() external returns (uint) {{ return {path}.priv(); }}
            }}"
        )
    };

    let (db, file) = make_test_db(&bodies("A"));
    for name in ["ok_inter", "ok_pub"] {
        let inference = named_function_inference(&db, file, name);
        assert!(
            inference.diagnostics.is_empty(),
            "{name}: {:?}",
            codes(&inference)
        );
    }
    let ext = named_function_inference(&db, file, "bad_ext");
    assert!(
        ext.diagnostics.iter().any(|d| {
            matches!(d, SemanticDiagnostic::InferenceFailure { message, .. }
                if message.contains("declaration-only"))
        }),
        "bad_ext: {:?}",
        codes(&ext)
    );
    let priv_ = named_function_inference(&db, file, "bad_priv");
    assert!(
        priv_.diagnostics.iter().any(|d| {
            matches!(d, SemanticDiagnostic::UnresolvedMember { member, .. } if member == "priv")
        }),
        "bad_priv: {:?}",
        codes(&priv_)
    );

    let (db, file) = make_test_db(&bodies("super"));
    for name in ["ok_inter", "ok_pub"] {
        let inference = named_function_inference(&db, file, name);
        assert!(
            inference.diagnostics.is_empty(),
            "{name}: {:?}",
            codes(&inference)
        );
    }
    for (name, member) in [("bad_ext", "ext"), ("bad_priv", "priv")] {
        let inference = named_function_inference(&db, file, name);
        assert!(
            inference.diagnostics.iter().any(|d| {
                matches!(d, SemanticDiagnostic::UnresolvedMember { member: m, .. } if *m == member)
            }),
            "{name}: {:?}",
            codes(&inference)
        );
    }
}

/// Inherited overloads share one candidate set: a `private` overload in a
/// nearer base does not hide the farther accessible one, and a bare internal
/// call selects the same declaration a value call does.
#[test]
fn inherited_calls_resolve_through_one_candidate_set() {
    let (db, file) = make_test_db(
        r#"
            contract A {
                function f(uint a) private returns (uint) { return a; }
            }
            contract B {
                function f(bool a) public returns (uint) { return 1; }
            }
            contract C is A, B {
                function g() external returns (uint) { return f(true); }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "g");
    assert!(inference.diagnostics.is_empty(), "{:?}", codes(&inference));
    assert!(
        expr_results(&inference)
            .any(|(_, ty, _)| { matches!(ty.kind(&db), TyKind::Primitive(Primitive::Uint(256))) }),
        "the accessible `f(bool)` should be selected"
    );

    let (db, file) = make_test_db(
        r#"
            contract A {
                function f(uint8) public pure virtual returns (uint8) { return 8; }
            }
            contract B {
                function f(uint256) public pure returns (uint256) { return 256; }
            }
            contract D is B, A {
                function bare() external pure returns (uint256) { return f(300); }
                function throughValue(D d) external view returns (uint256) { return d.f(300); }
            }
            "#,
    );

    for function in ["bare", "throughValue"] {
        let inference = named_function_inference(&db, file, function);
        assert!(
            inference.diagnostics.is_empty(),
            "{function}: {:?}",
            codes(&inference)
        );
        let selected_uint256 = expr_results(&inference).any(|(_, ty, resolution)| {
            if !matches!(ty.kind(&db), TyKind::Primitive(Primitive::Uint(256))) {
                return false;
            }
            let Some(Resolution::Callable(callable)) = resolution else {
                return false;
            };
            let Some(DefId::Function(id)) = callable.def() else {
                return false;
            };
            db.function_signature(id).is_some_and(|signature| {
                signature.params.first().is_some_and(|parameter| {
                    matches!(
                        parameter.ty().kind(&db),
                        TyKind::Primitive(Primitive::Uint(256))
                    )
                })
            })
        });
        assert!(
            selected_uint256,
            "{function} should select the uint256 declaration"
        );
    }
}

/// A base's private member named like a builtin does not block the builtin,
/// and a contract-level `external` does not block a file-level declaration.
#[test]
fn denied_members_do_not_shadow_builtins_or_file_level_names() {
    let (db, file) = make_test_db(
        r#"
            contract A {
                function keccak256(uint a) private returns (uint) { return a; }
            }
            contract C is A {
                function g() external returns (bytes32) { return keccak256("a"); }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "g");
    assert!(inference.diagnostics.is_empty(), "{:?}", codes(&inference));

    let (db, file) = make_test_db(
        r#"
            function log(uint a) pure {}
            contract C {
                function log(bool a) external {}
                function g() external { log(1); }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "g");
    assert!(inference.diagnostics.is_empty(), "{:?}", codes(&inference));
}

/// A value receiver exposes only the external interface — `this.f` rejects
/// `internal` and `private` members even inside the declaring contract.
#[test]
fn contract_value_members_follow_the_access_matrix() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function pub_() public {}
                function ext() external {}
                function inter() internal {}
                function priv() private {}
                function ok() external { this.pub_(); this.ext(); }
                function bad_inter() external { this.inter(); }
                function bad_priv() external { this.priv(); }
            }
            "#,
    );
    let ok = named_function_inference(&db, file, "ok");
    assert!(ok.diagnostics.is_empty(), "{:?}", codes(&ok));
    for (name, member) in [("bad_inter", "inter"), ("bad_priv", "priv")] {
        let inference = named_function_inference(&db, file, name);
        assert!(
            inference.diagnostics.iter().any(|d| {
                matches!(d, SemanticDiagnostic::UnresolvedMember { member: m, .. } if *m == member)
            }),
            "{name}: {:?}",
            codes(&inference)
        );
    }
}

/// Base-path function references expose `.selector` but never `.address` —
/// matching solc (`super.f.selector` and `A.f.selector` are legal).
#[test]
fn base_path_function_members() {
    let (db, file) = make_test_db(
        r#"
            abstract contract A {
                function pub_() public {}
                function pending() public;
                function ext() external {}
            }
            abstract contract C is A {
                function ownPub() public {}
                function ownPending() public;
                function g() external returns (bytes4, bytes4, bytes4, bytes4, bytes4, bytes4) {
                    return (
                        A.pub_.selector,
                        super.pub_.selector,
                        this.pub_.selector,
                        A.pending.selector,
                        A.ext.selector,
                        C.ownPending.selector
                    );
                }
                function bad() external returns (address) { return A.pub_.address; }
                function bad_own_selector() external returns (bytes4) { return C.ownPub.selector; }
                function bad_call() external returns (uint) { return A.pending(); }
            }
            "#,
    );
    let ok = named_function_inference(&db, file, "g");
    assert!(ok.diagnostics.is_empty(), "{:?}", codes(&ok));

    let bad = named_function_inference(&db, file, "bad");
    assert!(
        bad.diagnostics.iter().any(|d| {
            matches!(d, SemanticDiagnostic::UnresolvedMember { member, .. } if member == "address")
        }),
        "{:?}",
        codes(&bad)
    );

    let own_selector = named_function_inference(&db, file, "bad_own_selector");
    assert!(own_selector.diagnostics.iter().any(|d| {
        matches!(d, SemanticDiagnostic::UnresolvedMember { member, .. } if member == "selector")
    }));

    let non_invocable = named_function_inference(&db, file, "bad_call");
    assert!(non_invocable.diagnostics.iter().any(|d| {
        matches!(d, SemanticDiagnostic::InferenceFailure { message, .. }
            if message.contains("declaration-only"))
    }));
}

/// Reporting lookups retain failures as diagnostics through inference, without
/// turning inaccessible declarations into successful semantic resolutions.
#[test]
fn resolver_failures_accumulate_until_inference_finalization() {
    let (db, file) = make_test_db(
        r#"
            contract A {
                function internalOnly() internal {}
                function privateOnly() private {}
                function visible() external {}
            }
            contract C {
                function check(A a) external {
                    a.internalOnly();
                    a.visible();
                    a.privateOnly();
                    a.unknown();
                }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "check");
    let misses: Vec<_> = inference
        .diagnostics
        .iter()
        .filter_map(|diagnostic| match diagnostic {
            SemanticDiagnostic::UnresolvedMember {
                member, denials, ..
            } => Some((member.as_str(), denials.as_slice())),
            _ => None,
        })
        .collect();

    assert_eq!(misses.len(), 3, "{:?}", codes(&inference));
    assert_eq!(
        misses
            .iter()
            .find(|(name, _)| *name == "internalOnly")
            .unwrap()
            .1
            .len(),
        1
    );
    assert_eq!(
        misses
            .iter()
            .find(|(name, _)| *name == "privateOnly")
            .unwrap()
            .1
            .len(),
        1
    );
    assert!(
        misses
            .iter()
            .find(|(name, _)| *name == "unknown")
            .unwrap()
            .1
            .is_empty()
    );

    let resolutions = expr_results(&inference)
        .filter_map(|(_, _, resolution)| resolution)
        .collect::<Vec<_>>();
    assert!(!resolutions.iter().any(|resolution| matches!(
        resolution,
        Resolution::Callable(callable)
            if callable.def().is_some_and(|def| matches!(def, DefId::Function(id)
                if ["internalOnly", "privateOnly"].contains(&db.function_data(id).name.as_str())))
    )));
}
