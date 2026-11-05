use super::*;
use crate::hir::builtins::BuiltinFn;
use crate::hir::resolution::Callable;

/// An overload set is usable as a callee but is not a value — solc's "No
/// matching declaration found after variable lookup".
#[test]
fn bare_overload_set_is_reported_but_a_call_is_not() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function f(uint x) internal pure returns (uint) { return x; }
                function f(bool b) internal pure returns (uint) { return b ? 1 : 0; }
                function ok() public pure returns (uint) { return f(1); }
                function bad() public pure { f; }
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
            .collect::<Vec<_>>(),
    );

    let bad = named_function_inference(&db, file, "bad");
    assert!(
        bad.diagnostics.iter().any(|diagnostic| matches!(
            diagnostic,
            SemanticDiagnostic::InferenceFailure { message, .. }
                if message.contains("no matching declaration")
        )),
        "{:?}",
        bad.diagnostics
            .iter()
            .map(SemanticDiagnostic::code)
            .collect::<Vec<_>>(),
    );
}

/// Across an external ABI boundary — `this.e`, `c.e`, a public-library
/// delegatecall, `abi.encodeCall` — a `calldata` parameter accepts `memory`
/// (and `calldata`) arguments, because the call encodes them. An `internal`
/// library function is a jump, so the same argument stays rejected.
#[test]
fn external_calls_normalize_calldata_parameters_to_memory() {
    let accepted = [
        // `this.e(m)` and `this.e(c)` — message call, args encode.
        "contract C { function e(bytes calldata b) external pure {} \
         function f(bytes memory m, bytes calldata c) external pure { this.e(m); this.e(c); } }",
        // `c.e(m)` — an interface instance call.
        "interface E { function e(bytes calldata b) external returns (uint); } \
         contract C { function f(E e, bytes memory m) external { e.e(m); } }",
        // `L.f(m)` — a *public* library function is a delegatecall.
        "library L { function f(bytes calldata b) public pure returns (uint) { return b.length; } } \
         contract C { function f(bytes memory m) external pure { L.f(m); } }",
        // `abi.encodeCall` is itself the boundary.
        "interface E { function e(bytes calldata b) external returns (uint); } \
         contract C { function f(bytes memory m) external { abi.encodeCall(E.e, (m)); } }",
    ];
    for source in accepted {
        let (db, file) = make_test_db(source);
        let inference = named_function_inference(&db, file, "f");
        assert!(
            inference.diagnostics.is_empty(),
            "{source}: {:?}",
            codes(&inference)
        );
    }

    // `L.f(m)` where `f` is `internal` — a jump, so `calldata` stays strict.
    assert!(reports_type_mismatch(
        "library L { function f(bytes calldata b) internal pure returns (uint) { return b.length; } } \
         contract C { function f(bytes memory m) external pure { L.f(m); } }",
        "f"
    ));

    // An external *reference* is normalized too: `this.e` for a `bytes
    // calldata` declaration types as `function(bytes memory) external`, so it
    // fills a `memory` pointer (solc accepts `take(this.e)`).
    let (db, file) = make_test_db(
        "contract C { function e(bytes calldata b) external pure {} \
         function take(function(bytes memory) external pure ptr) internal {} \
         function f() external { take(this.e); } }",
    );
    let inference = named_function_inference(&db, file, "f");
    assert!(inference.diagnostics.is_empty(), "{:?}", codes(&inference));

    // But a declared `function(bytes calldata) external` pointer is called at
    // its *written* type — solc normalizes the reference, not the call, so
    // `p(m)` still rejects (verified).
    assert!(reports_type_mismatch(
        "contract C { function f(function(bytes calldata) external pure p, \
         bytes memory m) external pure { p(m); } }",
        "f"
    ));
}

/// solc's encodeCall target rule: "Expected regular external function type,
/// or external view on public function" — `this.f`/`c.f` are external views,
/// `C.e`/`E.e` are the external function type when the declaration is
/// `external`, an external pointer is fine; a library function is banned
/// outright ("Cannot use library functions").
#[test]
fn encode_call_accepts_only_external_targets() {
    let accepted = [
        "contract C { function e(bytes memory b) external {} \
         function f(bytes memory m) external { abi.encodeCall(this.e, (m)); } }",
        "contract C { function e(bytes memory b) external {} \
         function f(bytes memory m) external { abi.encodeCall(C.e, (m)); } }",
        "contract C { function p(bytes memory b) public {} \
         function f(bytes memory m) external { abi.encodeCall(this.p, (m)); } }",
        "contract C { function f(function(bytes memory) external p, bytes memory m) external { \
             abi.encodeCall(p, (m)); } }",
    ];
    for source in accepted {
        let (db, file) = make_test_db(source);
        let inference = named_function_inference(&db, file, "f");
        assert!(
            inference.diagnostics.is_empty(),
            "{source}: {:?}",
            codes(&inference)
        );
    }

    let rejected = [
        // an internal *reference* to a public function is not an external view
        "contract C { function p() public {} \
         function f() external { abi.encodeCall(C.p, ()); } }",
        "contract C { function p() public {} \
         function f() external { abi.encodeCall(p, ()); } }",
        "contract C { function q() internal {} \
         function f() external { abi.encodeCall(q, ()); } }",
        // library functions are banned outright, whatever the flavour
        "library L { function f(bytes memory b) public pure returns (uint) { return b.length; } } \
         contract C { function f() external pure { abi.encodeCall(L.f, (\"x\")); } }",
        "library L { function f(bytes memory b) internal pure returns (uint) { return b.length; } } \
         contract C { function f() external pure { abi.encodeCall(L.f, (\"x\")); } }",
        // an internal pointer is not an external function type
        "contract C { function f(function() internal p) internal { \
             abi.encodeCall(p, ()); } }",
    ];
    for source in rejected {
        let (db, file) = make_test_db(source);
        let inference = named_function_inference(&db, file, "f");
        assert!(
            inference
                .diagnostics
                .iter()
                .any(|d| matches!(d, SemanticDiagnostic::InferenceFailure { .. })),
            "{source} must reject: {:?}",
            codes(&inference)
        );
    }
}

/// A value-typed callee with no resolution (`a[0]` where `a` is `uint[]`) must
/// report; a `FnPtr` element is valid Solidity and still a tracked gap.
#[test]
fn non_function_callee_without_a_resolution_is_reported() {
    let (db, file) =
        make_test_db("contract C { function f(uint[] memory a) external { a[0](1); } }");
    let inference = function_inference(&db, file);
    assert!(
        inference.diagnostics.iter().any(|diagnostic| matches!(
            diagnostic,
            SemanticDiagnostic::InferenceFailure { message, .. }
                if message.contains("not callable")
        )),
        "{:?}",
        codes(&inference),
    );
}

/// A failing callee reports once: `Expr::Call` must not add "expression is
/// not callable" on top of the callee's own diagnostic, a bare unresolved
/// expression still lands a diagnostic instead of an empty inferred value,
/// and an unavailable signature propagates its `UnresolvedType` to the call.
#[test]
fn callee_failures_do_not_cascade() {
    let (db, file) = make_test_db("contract C { function f() external { missing(1); } }");
    let inference = function_inference(&db, file);
    let unresolved = inference
        .diagnostics
        .iter()
        .filter(|diagnostic| matches!(diagnostic, SemanticDiagnostic::UnresolvedName { .. }))
        .count();
    assert_eq!(unresolved, 1, "{:?}", codes(&inference));
    assert!(
        !inference.diagnostics.iter().any(|diagnostic| matches!(
            diagnostic,
            SemanticDiagnostic::InferenceFailure { message, .. }
                if message.contains("not callable")
        )),
        "{:?}",
        codes(&inference),
    );

    let (db, file) = make_test_db("contract C { function f() external { missingName; } }");
    let inference = function_inference(&db, file);
    assert!(inference.diagnostics.iter().any(|diagnostic| {
        matches!(diagnostic, SemanticDiagnostic::UnresolvedName { name, .. } if name == "missingName")
    }));

    let (db, file) = make_test_db(
        "contract C { function f(Missing value) internal {} function g() external { f(); } }",
    );
    let inference = last_function_inference(&db, file);
    assert!(inference.diagnostics.iter().any(|diagnostic| {
        matches!(diagnostic, SemanticDiagnostic::UnresolvedType { .. })
            || matches!(diagnostic, SemanticDiagnostic::InferenceFailure { .. })
    }));
}

/// A bare `f` reference types as a function (a `TyKind::Function` value);
/// `f()` types as the declared return. The distinction is what makes
/// references assignable to pointers and calls assignable to their result.
#[test]
fn a_reference_types_as_the_function_a_call_as_the_return() {
    let (db, file) = make_test_db(
        "contract C { function f() public returns (uint) { return 1; } function g() external { f; uint x = f(); } }",
    );
    let inference = last_function_inference(&db, file);
    assert!(
        expr_results(&inference).any(|(_, ty, resolution)| {
            matches!(resolution, Some(Resolution::Callable(_)))
                && matches!(ty.kind(&db), TyKind::Function { .. })
        }),
        "a bare reference should keep a function-pointer type"
    );
    assert!(
        expr_results(&inference)
            .any(|(_, ty, _)| matches!(ty.kind(&db), TyKind::Primitive(Primitive::Uint(_)))),
        "the call should type as the declared return"
    );
}

#[test]
fn overload_resolution_selects_the_only_convertible_candidate() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function f(uint8 a) public pure returns (uint8) { return a; }
                function f(uint32 a) public pure returns (uint32) { return a; }
                function g() external { uint16 y = 3; f(y); }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "g");
    // uint16 → uint8 is not an implicit conversion, uint16 → uint32 is,
    // so the uint32 overload must win and the call must type as uint32.
    let call_result = expr_results(&inference).find_map(|(_, ty, resolution)| match resolution {
        Some(Resolution::Callable(c))
            if c.bound() == 0 && matches!(ty.kind(&db), TyKind::Primitive(Primitive::Uint(32))) =>
        {
            Some(())
        }
        _ => None,
    });
    assert!(
        call_result.is_some(),
        "expected the uint32 overload to be selected"
    );
    assert!(!inference.diagnostics.iter().any(|d| {
        matches!(d, SemanticDiagnostic::AmbiguousOverload { .. })
            || matches!(d, SemanticDiagnostic::NoMatchingOverload { .. })
    }));
}

#[test]
fn ambiguous_overload_keeps_callable_candidates() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function f(uint8 a) public pure returns (uint8) { return a; }
                function f(uint16 a) public pure returns (uint16) { return a; }
                function g() external { f(1); }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "g");
    assert!(
        inference.diagnostics.iter().any(|diagnostic| {
            matches!(diagnostic, SemanticDiagnostic::AmbiguousOverload { .. })
        })
    );
    // The call reports its own ambiguity; it must not also collect the
    // bare-overload-set failure from the `Rvalue` check.
    assert!(
        !inference.diagnostics.iter().any(|diagnostic| matches!(
            diagnostic,
            SemanticDiagnostic::InferenceFailure { message, .. }
                if message.contains("no matching declaration")
        )),
        "{:?}",
        codes(&inference),
    );
    assert!(expr_results(&inference).any(|(_, _, resolution)| {
        matches!(resolution, Some(Resolution::Callables(callables)) if callables.len() == 2)
    }));
}

#[test]
fn type_mismatch_reports_conversion_failure() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function f(uint8 a) public pure returns (uint8) { return a; }
                function g() external { f(300); }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "g");
    assert!(
        inference
            .diagnostics
            .iter()
            .any(|diagnostic| { matches!(diagnostic, SemanticDiagnostic::TypeMismatch { .. }) })
    );
}

/// A multi-value return types as a tuple — declared returns and the builtin
/// `(bool, bytes)` of `address.call` alike.
#[test]
fn multi_value_returns_type_as_tuples() {
    for (source, function) in [
        (
            r#"
            contract C {
                function f() public pure returns (uint8, uint16) { return (1, 2); }
                function g() external { f(); }
            }
            "#,
            "g",
        ),
        (
            r#"
            contract C {
                function f(address a, bytes memory data) external { a.call(data); }
            }
            "#,
            "f",
        ),
    ] {
        let (db, file) = make_test_db(source);
        let inference = named_function_inference(&db, file, function);
        assert!(
            expr_results(&inference).any(|(_, ty, _)| {
                matches!(ty.kind(&db), TyKind::Tuple(tuple) if tuple.elements.len() == 2)
            }),
            "{source}: expected the call to type as a two-element tuple"
        );
    }
}

#[test]
fn call_checker_selects_without_running_inference() {
    let (db, file) = make_test_db("contract C { function f(uint a, uint b) external {} }");
    let callable = Callable::new_direct(find_function(&db, file, "f"));
    let uint = Type::primitive(&db, Primitive::Uint(256));
    let sig = Fn {
        params: Box::new([uint, uint]),
        ..Fn::default()
    };

    // Arity and parameter lookup are decisions over the signature alone —
    // no `SyntaxStore`, no tables, no sink.
    assert!(Inferer::arg_counts_match(&sig, &callable, 2));
    assert!(!Inferer::arg_counts_match(&sig, &callable, 1));
    let unbound = Inferer::unbound_params(&sig, &callable).expect("no receiver");
    assert_eq!(
        Inferer::param_for(&callable, unbound, 1).map(|ty| ty.display(&db)),
        Some("uint256".to_string())
    );
    assert!(Inferer::param_for(&callable, unbound, 2).is_none());
}

#[test]
fn function_pointer_calls_dispatch_on_the_pointer_type() {
    // `local` and `s.fp` have no callable declaration — they are function
    // *values*, so the call is keyed on `TyKind::FnPtr`, not a resolution.
    let (db, file) = make_test_db(
        r#"
            contract C {
                struct S { function (uint) internal returns (uint) fp; }
                function inc(uint x) internal pure returns (uint) { return x + 1; }
                function f() external returns (uint) {
                    function (uint) internal returns (uint) local = inc;
                    S memory s;
                    s.fp = inc;
                    return local(1) + s.fp(2);
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
    assert!(
        expr_results(&inference)
            .any(|(_, ty, _)| { matches!(ty.kind(&db), TyKind::Primitive(Primitive::Uint(256))) }),
        "the pointer calls should produce the pointer's return type"
    );
}

#[test]
fn direct_contract_type_call_is_rejected_as_declaration_only() {
    // `C.f()` inside `C` is a legal self-qualified internal call — the
    // declaration-only rejection is for a contract that is not the caller or
    // one of its bases (solc: "Cannot call function via contract type name").
    let (db, file) = make_test_db(
        r#"
            contract X {
                function f() public {}
            }
            contract C {
                function g() external { X.f(); }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "g");
    assert!(inference.diagnostics.iter().any(|diagnostic| {
        matches!(diagnostic, SemanticDiagnostic::InferenceFailure { message, .. }
            if message.contains("declaration-only"))
    }));
}

/// A cast result is a runtime value, so its declared members are ordinary
/// invocable callables — not declaration-only namespace members — for
/// interfaces and contracts alike.
#[test]
fn cast_member_calls_are_invocable() {
    for (source, member) in [
        (
            r#"
            interface IERC20 {
                function transfer(address to, uint256 amount) external returns (bool);
            }
            contract C {
                function f(address token, address to) external {
                    IERC20(token).transfer(to, 1);
                }
            }
            "#,
            "transfer",
        ),
        (
            r#"
            contract Target {
                function ping() external {}
            }
            contract C {
                function f(address target) external {
                    Target(target).ping();
                }
            }
            "#,
            "ping",
        ),
    ] {
        let (db, file) = make_test_db(source);
        let inference = named_function_inference(&db, file, "f");
        assert!(!inference.diagnostics.iter().any(|diagnostic| {
            matches!(diagnostic, SemanticDiagnostic::InferenceFailure { message, .. }
                if message.contains("declaration-only"))
        }));
        assert!(!inference.diagnostics.iter().any(|diagnostic| {
            matches!(diagnostic, SemanticDiagnostic::UnresolvedMember { member: m, .. }
                if m == member)
        }));
        assert!(expr_results(&inference).any(|(_, _, resolution)| {
            matches!(resolution, Some(Resolution::Callable(callable)) if callable.bound() == 0)
        }));
    }
}

/// `emit E(..)` and `revert E(..)` — the *call* is `Void`; the declaration
/// type lives on the `Callable` resolution (Solar: `type_of(E(..))` is
/// `unit`). `emit` also keeps its event callable resolution.
#[test]
fn emit_and_revert_calls_type_as_void() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                event Transfer(address to, uint256 value);
                error Unauthorized(address caller);
                function f() external {
                    emit Transfer(msg.sender, 1);
                    revert Unauthorized(msg.sender);
                }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "f");
    assert!(
        inference.diagnostics.is_empty(),
        "{:?}",
        codes(&inference)
    );
    assert!(expr_results(&inference).any(|(_, ty, resolution)| {
        matches!(resolution, Some(Resolution::Callable(c))
            if matches!(c.def(), Some(DefId::Event(_))))
            && matches!(ty.kind(&db), TyKind::Void)
    }));
    assert!(expr_results(&inference).any(|(_, ty, resolution)| {
        matches!(resolution, Some(Resolution::Callable(c))
            if matches!(c.def(), Some(DefId::Error(_))))
            && matches!(ty.kind(&db), TyKind::Void)
    }));
}

#[test]
fn event_call_still_checks_arguments() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                event Transfer(address to, uint256 value);
                function f() external {
                    emit Transfer("oops", 1);
                }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "f");
    assert!(
        inference
            .diagnostics
            .iter()
            .any(|diagnostic| { matches!(diagnostic, SemanticDiagnostic::TypeMismatch { .. }) })
    );
}

#[test]
fn modifier_invocation_produces_no_diagnostics() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                modifier onlyOwner(address who) { _; }
                function f() external onlyOwner(msg.sender) {}
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
}

/// The three category-constrained invocations: `emit`'s callee must denote an
/// event, `revert`'s an error, and a modifier invocation's a modifier. Each
/// requirement is supplied by the wrapper around the call, so a nested call in
/// the arguments keeps its ordinary requirement.
#[test]
fn invocation_category_is_checked_at_the_statement() {
    let cases: &[(&str, &str)] = &[
        (
            "contract C { function f() public { emit f(); } }",
            "an event",
        ),
        (
            "contract C { function f() public { revert f(); } }",
            "an error",
        ),
        (
            "contract C { function g() internal {} function f() external g() {} }",
            "a modifier",
        ),
    ];
    for (source, what) in cases {
        let (db, file) = make_test_db(source);
        let inference = named_function_inference(&db, file, "f");
        assert!(
            inference.diagnostics.iter().any(|d| matches!(d,
                SemanticDiagnostic::InferenceFailure { message, .. }
                if message.contains(what))),
            "{source}: expected `{what}`: {:?}",
            codes(&inference),
        );
    }
}

/// An error is only constructed in `revert E(..)` or the second argument of
/// `require(c, E(..))` — Solar's `in_revert`. Anywhere else it reports the
/// position error and types as `Invalid` instead of `Void`, including when it
/// is nested inside the arguments of an enclosing `revert` or `require`.
#[test]
fn error_construction_is_restricted_to_revert_and_require() {
    let cases = [
        "contract C { error E(); function f() external { E(); } }",
        "contract C { error E(uint x); function f() external { uint x = E(1); } }",
        "contract C { error E(); function g(uint x) internal {} function f() external { g(E()); } }",
        "contract C { error E(uint x); function f() external { revert E(E(1)); } }",
        "contract C { error E(uint x); function f(bool c) external { require(c, E(E(1))); } }",
        "contract C { error E(); function f() external { require(E(), \"bad\"); } }",
    ];
    for source in cases {
        let (db, file) = make_test_db(source);
        let inference = named_function_inference(&db, file, "f");
        assert!(
            inference.diagnostics.iter().any(|d| matches!(
                d,
                SemanticDiagnostic::InferenceFailure { message, .. }
                    if message.contains("errors can only be used with revert statements or require functions")
            )),
            "{source}: {:?}",
            codes(&inference),
        );
    }
}

/// `abi.decode` returns the named types as a tuple — or, with a single
/// element, the element itself — with reference elements upcast to `memory`
/// (the ABI boundary). `uint256[]` in the type tuple is array-type syntax:
/// the base resolves to a type name, so indexing never runs.
#[test]
fn abi_decode_produces_the_named_types() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function f(bytes memory data) external {
                    abi.decode(data, (uint256, address));
                    abi.decode(data, (string));
                    abi.decode(data, (uint256[]));
                }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "f");
    assert!(
        inference.diagnostics.is_empty(),
        "{:?}",
        codes(&inference)
    );
    assert!(
        expr_results(&inference).any(|(_, ty, resolution)| {
            matches!(resolution, Some(Resolution::Callable(c))
            if matches!(c.builtin(), Some(BuiltinFn::Decode)))
                && matches!(ty.kind(&db), TyKind::Tuple(tuple) if tuple.elements.len() == 2)
        }),
        "expected the decode call to produce a two-element tuple"
    );
    assert!(
        expr_results(&inference).any(|(_, ty, _)| {
            matches!(ty.kind(&db), TyKind::Ref(inner, Location::Memory)
            if matches!(inner.kind(&db), TyKind::Primitive(Primitive::String)))
        }),
        "expected a single-element decode to produce `string memory`"
    );
    assert!(
        expr_results(&inference).any(|(_, ty, _)| {
            matches!(ty.kind(&db), TyKind::Ref(inner, Location::Memory)
            if matches!(inner.kind(&db), TyKind::Array { .. }))
        }),
        "expected a single-element decode to produce `uint256[] memory`"
    );
}

#[test]
fn call_options_are_validated_and_do_not_drop_the_call() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function f(uint256 a) external payable returns (uint256) { return a; }
                function h(uint256 a) internal returns (uint256) { return a; }
                function ok() external payable returns (uint256) {
                    return this.f{value: 1}(2);
                }
                function internal_options() external payable returns (uint256) {
                    return h{value: 1}(2);
                }
                function unknown_option() external payable returns (uint256) {
                    return this.f{foo: 1}(2);
                }
                function builtin_options(address a) external payable {
                    a.call{value: 1}("");
                }
                function salty() external payable returns (uint256) {
                    return this.f{salt: bytes32(0)}(2);
                }
                function mistyped() external payable returns (uint256) {
                    return this.f{value: "x"}(2);
                }
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
    assert!(
        expr_results(&ok)
            .any(|(_, ty, _)| { matches!(ty.kind(&db), TyKind::Primitive(Primitive::Uint(256))) }),
        "expected `this.f{{value: 1}}(2)` to type as the call result"
    );

    // A builtin reached through an instance (`address.call`) carries the entry's
    // own `callkind`, so it is an external function value and accepts options.
    // Verified against solc 0.8.37: `a.call{value: 1}("")` compiles.
    let builtin = named_function_inference(&db, file, "builtin_options");
    assert!(
        builtin.diagnostics.is_empty(),
        "{:?}",
        builtin
            .diagnostics
            .iter()
            .map(SemanticDiagnostic::code)
            .collect::<Vec<_>>()
    );

    let internal = named_function_inference(&db, file, "internal_options");
    assert!(
        internal.diagnostics.iter().any(|d| matches!(d,
            SemanticDiagnostic::InferenceFailure { message, .. }
            if message.contains("external function calls"))),
        "{:?}",
        internal
            .diagnostics
            .iter()
            .map(SemanticDiagnostic::code)
            .collect::<Vec<_>>()
    );

    let unknown = named_function_inference(&db, file, "unknown_option");
    assert!(
        unknown.diagnostics.iter().any(|d| matches!(d,
            SemanticDiagnostic::InferenceFailure { message, .. }
            if message.contains("unknown call option"))),
        "{:?}",
        unknown
            .diagnostics
            .iter()
            .map(SemanticDiagnostic::code)
            .collect::<Vec<_>>()
    );

    // `salt` exists only on a contract creation.
    let salty = named_function_inference(&db, file, "salty");
    assert!(
        salty.diagnostics.iter().any(|d| matches!(d,
            SemanticDiagnostic::InferenceFailure { message, .. }
            if message.contains("`salt` can only be used with `new`"))),
        "{:?}",
        codes(&salty)
    );

    // Each option value checks against the type `call_options_for` publishes
    // — `uint256` for `value`.
    let mistyped = named_function_inference(&db, file, "mistyped");
    assert!(
        mistyped
            .diagnostics
            .iter()
            .any(|d| matches!(d, SemanticDiagnostic::TypeMismatch { .. })),
        "{:?}",
        codes(&mistyped)
    );
}

/// An overloaded callee stays an overload set through `{value: …}` — the
/// enclosing call's arguments select the member, and only then is the
/// externality of the selected callable decided. When the set holds no
/// external member, the internal call with `value` is rejected.
#[test]
fn call_options_apply_to_the_selected_overload() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function f(uint256 a) external payable returns (uint256) { return a; }
                function f(bytes32 a) external returns (bytes32) { return a; }
                function ok() external payable returns (uint256) {
                    return this.f{value: 1}(uint256(2));
                }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "ok");
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
        expr_results(&inference)
            .any(|(_, ty, _)| { matches!(ty.kind(&db), TyKind::Primitive(Primitive::Uint(256))) }),
        "expected `this.f{{value: 1}}(2)` to select the `uint256` overload"
    );

    let (db, file) = make_test_db(
        r#"
            contract C {
                function f(uint256 a) internal returns (uint256) { return a; }
                function f(bytes32 a) internal returns (bytes32) { return a; }
                function bad() external {
                    f{value: 1}(uint256(2));
                }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "bad");
    assert!(
        inference.diagnostics.iter().any(|d| matches!(d,
            SemanticDiagnostic::InferenceFailure { message, .. }
            if message.contains("external function calls"))),
        "{:?}",
        inference
            .diagnostics
            .iter()
            .map(SemanticDiagnostic::code)
            .collect::<Vec<_>>()
    );
}

/// `value` attaches only to a payable target — solc's "Cannot set option
/// 'value' on a non-payable function type" — checked on the member the
/// call selected: a resolved function and a function pointer alike.
#[test]
fn call_options_value_requires_a_payable_callee() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function f(uint256 a) external returns (uint256) { return a; }
                function g() external payable returns (uint256) {
                    return this.f{value: 1}(2);
                }
                function h(function() external p) external {
                    p{value: 1}();
                }
            }
            "#,
    );
    for name in ["g", "h"] {
        let inference = named_function_inference(&db, file, name);
        assert!(
            inference.diagnostics.iter().any(|d| matches!(d,
                SemanticDiagnostic::InferenceFailure { message, .. }
                if message.contains("non-payable"))),
            "{name}: {:?}",
            codes(&inference)
        );
    }
}

/// The bare-call address members are the only builtin option hosts, each
/// with its own admitted set — `call` takes `value` and `gas`, while
/// `delegatecall`/`staticcall` cannot move value. `send`/`transfer` admit
/// nothing: every option they are given is rejected by name.
#[test]
fn call_options_on_builtins_follow_their_option_sets() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function bare_calls(address a) external {
                    a.delegatecall{gas: 1000}("");
                    a.staticcall{gas: 1000}("");
                }
                function no_hosts(address payable a) external {
                    a.send{gas: 1}(1);
                    a.transfer{value: 1}(1);
                }
                function value_opts(address a) external {
                    a.delegatecall{value: 1}("");
                    a.staticcall{value: 1}("");
                }
            }
            "#,
    );

    let ok = named_function_inference(&db, file, "bare_calls");
    assert!(ok.diagnostics.is_empty(), "{:?}", codes(&ok));

    // `send`/`transfer` are external members but admit no options —
    // each option reports the dedicated builtin rejection.
    let no_hosts = named_function_inference(&db, file, "no_hosts");
    for (member, option) in [("send", "gas"), ("transfer", "value")] {
        assert!(
            no_hosts.diagnostics.iter().any(|d| matches!(d,
                SemanticDiagnostic::InferenceFailure { message, .. }
                if message.contains(&format!("`{option}` cannot be used with `{member}`")))),
            "{member}: {:?}",
            codes(&no_hosts)
        );
    }

    // `value` on a gas-only host reports the dedicated rejection — and not
    // the generic non-payable one alongside it.
    let value_opts = named_function_inference(&db, file, "value_opts");
    for member in ["delegatecall", "staticcall"] {
        assert!(
            value_opts.diagnostics.iter().any(|d| matches!(d,
                SemanticDiagnostic::InferenceFailure { message, .. }
                if message.contains(&format!("`value` cannot be used with `{member}`")))),
            "{member}: {:?}",
            codes(&value_opts)
        );
    }
    assert!(
        !value_opts.diagnostics.iter().any(|d| matches!(d,
            SemanticDiagnostic::InferenceFailure { message, .. }
            if message.contains("non-payable"))),
        "{:?}",
        codes(&value_opts)
    );
}

/// `require(bool)` / `require(bool, string)` / `require(bool, E(...))` are the
/// accepted shapes; every other argument list is rejected.
#[test]
fn require_checks_its_arguments() {
    let accepts = [
        r#"
            contract C {
                error Unauthorized(address caller);
                function f(bool c) external {
                    require(c, Unauthorized(msg.sender));
                }
            }
            "#,
        r#"
            contract C {
                function f(bool c) external {
                    require(c, "unauthorized");
                }
            }
            "#,
    ];
    for source in accepts {
        let (db, file) = make_test_db(source);
        let inference = named_function_inference(&db, file, "f");
        assert!(inference.diagnostics.is_empty(), "{:?}", codes(&inference));
    }

    let rejects: Rejects = &[
        // `require(c, E)` — an error *name* refers to a declaration, not an
        // instance; Solar's shape check requires a constructor call here.
        (
            r#"
            contract C {
                error Unauthorized(address caller);
                function f(bool c) external {
                    require(c, Unauthorized);
                }
            }
            "#,
            |d| matches!(d, SemanticDiagnostic::TypeMismatch { .. }),
        ),
        (
            r#"
            contract C {
                function f(bool c) external {
                    require(c, 42);
                }
            }
            "#,
            |d| matches!(d, SemanticDiagnostic::TypeMismatch { .. }),
        ),
        (
            r#"
            contract C {
                function f(uint256 c) external {
                    require(c);
                }
            }
            "#,
            |d| matches!(d, SemanticDiagnostic::TypeMismatch { .. }),
        ),
        (
            r#"
            contract C {
                function f(bool c) external {
                    require(c, "msg", 1);
                }
            }
            "#,
            |d| {
                matches!(
                    d,
                    SemanticDiagnostic::InvalidArgumentCount { actual: 3, .. }
                )
            },
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

/// `abi.decode(data, (T, ..))` — the data argument must be ABI-boundary `bytes`
/// and the second argument must be a tuple of *type names*.
/// `abi.decode` checks both arguments: the source must be `bytes`, the type
/// list must be type names — `()` is a valid empty list, and a call is
/// rejected even when it returns a tuple of type names (the check is on the
/// argument's *syntax*).
#[test]
fn abi_decode_checks_both_arguments() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function f() internal pure returns (uint, address) { return (1, address(0)); }
                function empty(bytes memory data) external pure { abi.decode(data, ()); }
            }
            "#,
    );
    let empty = named_function_inference(&db, file, "empty");
    assert!(empty.diagnostics.is_empty(), "{:?}", codes(&empty));

    let rejects: Rejects = &[
        (
            r#"
            contract C {
                function f(uint256 data) external {
                    abi.decode(data, (uint256));
                }
            }
            "#,
            |d| matches!(d, SemanticDiagnostic::TypeMismatch { .. }),
        ),
        (
            r#"
            contract C {
                function f(bytes memory data, uint256 x) external {
                    abi.decode(data, (x, x));
                }
            }
            "#,
            |d| {
                matches!(
                    d,
                    SemanticDiagnostic::InferenceFailure { message, .. }
                        if message.contains("type name")
                )
            },
        ),
        (
            r#"
            contract C {
                function f() internal pure returns (uint, address) { return (1, address(0)); }
                function bad(bytes memory data) external pure { abi.decode(data, f()); }
            }
            "#,
            |d| {
                matches!(
                    d,
                    SemanticDiagnostic::InferenceFailure { message, .. }
                        if message.contains("has to be a tuple of types")
                )
            },
        ),
    ];
    for (source, expected) in rejects {
        let (db, file) = make_test_db(source);
        let function = if source.contains("bad") { "bad" } else { "f" };
        let inference = named_function_inference(&db, file, function);
        assert!(
            inference.diagnostics.iter().any(expected),
            "{source}: {:?}",
            codes(&inference)
        );
    }
}

/// `bytes.concat`'s tail arity is `TypedTail`: trailing arguments convert to
/// the last declared parameter.
#[test]
fn bytes_concat_checks_its_typed_tail() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function f(bytes memory a, bytes memory b) external {
                    bytes.concat(a, b, hex"00");
                }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "f");
    assert!(inference.diagnostics.is_empty(), "{:?}", codes(&inference));

    let (db, file) = make_test_db(
        r#"
            contract C {
                function f(bytes memory a) external {
                    bytes.concat(a, 1);
                }
            }
            "#,
    );
    let inference = named_function_inference(&db, file, "f");
    assert!(
        inference
            .diagnostics
            .iter()
            .any(|d| matches!(d, SemanticDiagnostic::TypeMismatch { .. })),
        "{:?}",
        codes(&inference),
    );
}

/// A cast converts exactly one argument; a struct "constructor" is instead an
/// overload over its fields, so its arity is a separate check. solc reports
/// `uint(1, 2)` and `payable()` alike as "Exactly one argument expected for
/// explicit type conversion".
#[test]
fn a_cast_takes_exactly_one_argument() {
    let (db, file) = make_test_db(
        r#"
            struct S { uint a; uint b; }
            contract C {
                function too_many() external pure { uint(1, 2); }
                function none() external pure { payable(); }
                function ok() external view { address payable p = payable(msg.sender); }
                function struct_ctor() external pure { S memory s = S(1, 2); }
            }
            "#,
    );
    for name in ["too_many", "none"] {
        let inference = named_function_inference(&db, file, name);
        assert!(
            inference
                .diagnostics
                .iter()
                .any(|d| matches!(d, SemanticDiagnostic::InvalidArgumentCount { .. })),
            "{name}: {:?}",
            codes(&inference),
        );
    }
    for name in ["ok", "struct_ctor"] {
        let inference = named_function_inference(&db, file, name);
        assert!(
            inference.diagnostics.is_empty(),
            "{name}: {:?}",
            codes(&inference)
        );
    }
}

/// `view` may only call `view`/`pure`, and `pure` only `pure`; a non-`payable`
/// or `payable` body may call anything.
#[test]
fn callee_state_mutability_is_enforced() {
    let (db, file) = make_test_db(
        r#"
        contract C {
            uint state;
            function modifying() public { state = 1; }
            function reading() public view returns (uint) { return state; }
            function pureFn() public pure returns (uint) { return 1; }
            function viewOk() public view { reading(); pureFn(); }
            function viewBad() public view { modifying(); }
            function pureOk() public pure { pureFn(); }
            function pureBad() public pure { reading(); }
            function payableOk() public payable { modifying(); }
        }
        "#,
    );
    for name in ["viewOk", "pureOk", "payableOk"] {
        let inference = named_function_inference(&db, file, name);
        assert!(
            inference.diagnostics.is_empty(),
            "{name}: {:?}",
            codes(&inference)
        );
    }
    // Calling a non-`view` function modifies state; calling a `view` function
    // only reads, so a `pure` caller is short of `view`, not of `nonpayable`.
    for (name, required) in [
        ("viewBad", Mutability::NonPayable),
        ("pureBad", Mutability::View),
    ] {
        let inference = named_function_inference(&db, file, name);
        assert!(
            inference.diagnostics.iter().any(|d| matches!(d,
                SemanticDiagnostic::StateMutability { required: actual, .. } if *actual == required)),
            "{name}: {:?}",
            codes(&inference)
        );
    }
}

/// `pure` may not read state or the environment. A `constant` is not state,
/// and neither is `this` — solc lets a `pure` function name it (`this.f()`
/// and `this.balance` carry the access's effect, not the name).
#[test]
fn pure_rejects_state_and_environment_reads() {
    let (db, file) = make_test_db(
        r#"
        contract C {
            uint state;
            uint constant K = 1;
            function f() public pure returns (uint) { return state; }
            function g() public pure returns (address) { return msg.sender; }
            function h() public pure returns (uint) { return block.number; }
            function i() public pure returns (C) { return this; }
            function ok() public pure returns (uint) { return K; }
        }
        "#,
    );
    for name in ["f", "g", "h"] {
        let inference = named_function_inference(&db, file, name);
        assert!(
            inference.diagnostics.iter().any(|d| matches!(d,
                SemanticDiagnostic::StateMutability { required: Mutability::View, .. })),
            "{name}: {:?}",
            codes(&inference)
        );
    }
    for name in ["i", "ok"] {
        let inference = named_function_inference(&db, file, name);
        assert!(inference.diagnostics.is_empty(), "{name}: {:?}", codes(&inference));
    }
}

/// `msg.value` reads the call's attached value, so only a `payable` function
/// may observe it.
#[test]
fn msg_value_requires_a_payable_function() {
    let (db, file) = make_test_db(
        r#"
        contract C {
            function ok() public payable returns (uint) { return msg.value; }
            function bad() public returns (uint) { return msg.value; }
            function viewBad() public view returns (uint) { return msg.value; }
        }
        "#,
    );
    let ok = named_function_inference(&db, file, "ok");
    assert!(ok.diagnostics.is_empty(), "{:?}", codes(&ok));
    for name in ["bad", "viewBad"] {
        let inference = named_function_inference(&db, file, name);
        assert!(
            inference.diagnostics.iter().any(|d| matches!(d,
                SemanticDiagnostic::InferenceFailure { message, .. }
                    if message.contains("msg.value"))),
            "{name}: {:?}",
            codes(&inference)
        );
    }
}

/// `abi.encodeCall(f, (args…))` takes a function reference and a tuple whose
/// components convert to that function's parameters.
#[test]
fn abi_encode_call_checks_the_target_and_its_arguments() {
    let (db, file) = make_test_db(
        r#"
        interface I { function transfer(address to, uint256 v) external returns (bool); }
        contract C {
            function ok(I token, address to) external view {
                bytes memory a = abi.encodeCall(I.transfer, (to, 1));
                bytes memory b = abi.encodeCall(token.transfer, (to, 1));
            }
            function wrongType() external view {
                abi.encodeCall(I.transfer, (true, 1));
            }
            function wrongArity() external view {
                abi.encodeCall(I.transfer, (1));
            }
            function notAFunction(uint x) external view {
                abi.encodeCall(x, (1));
            }
        }
        "#,
    );
    let ok = named_function_inference(&db, file, "ok");
    assert!(ok.diagnostics.is_empty(), "{:?}", codes(&ok));

    let wrong_type = named_function_inference(&db, file, "wrongType");
    assert!(
        wrong_type
            .diagnostics
            .iter()
            .any(|d| matches!(d, SemanticDiagnostic::TypeMismatch { .. })),
        "{:?}",
        codes(&wrong_type)
    );
    let wrong_arity = named_function_inference(&db, file, "wrongArity");
    assert!(
        wrong_arity
            .diagnostics
            .iter()
            .any(|d| matches!(d, SemanticDiagnostic::InvalidArgumentCount { .. })),
        "{:?}",
        codes(&wrong_arity)
    );
    let not_a_function = named_function_inference(&db, file, "notAFunction");
    assert!(
        not_a_function.diagnostics.iter().any(|d| matches!(d,
            SemanticDiagnostic::InferenceFailure { message, .. }
                if message.contains("must be a function pointer"))),
        "{:?}",
        codes(&not_a_function)
    );
}

/// One declaration of each named-capable kind, plus an overload pair to pin the
/// per-candidate binding.
const NAMED_FIXTURE: &str = r#"
    struct S { uint x; uint y; }
    contract B { constructor(uint a) { } }
    contract C {
        event Ev(uint a, uint b);
        error Er(uint a, uint b);
        function f(uint a, uint b) public pure returns (uint) { return a + b; }
        function ov(uint a) public pure returns (uint) { return a; }
        function ov(bool b) public pure returns (uint) { return b ? 1 : 0; }
        function h() external { @BODY@ }
    }
"#;

/// The diagnostic codes `BODY;` produces inside the fixture. Empty means the
/// call was accepted.
fn run(body: &str) -> Vec<DiagnosticCode> {
    let (db, file) = make_test_db(&NAMED_FIXTURE.replace("@BODY@", body));
    named_function_inference(&db, file, "h")
        .diagnostics
        .iter()
        .map(SemanticDiagnostic::code)
        .collect()
}

/// The `InferenceFailure` messages `BODY;` produces.
fn failures(body: &str) -> Vec<String> {
    let (db, file) = make_test_db(&NAMED_FIXTURE.replace("@BODY@", body));
    named_function_inference(&db, file, "h")
        .diagnostics
        .iter()
        .filter_map(|diagnostic| match diagnostic {
            SemanticDiagnostic::InferenceFailure { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

fn accepted(body: &str) {
    let codes = run(body);
    assert!(
        codes.is_empty(),
        "`{body}` should be accepted, got {codes:?}"
    );
}

/// Every case is verified against solc 0.8.34. The group is one expression,
/// so the call sees one argument — which is what keeps a named call from
/// being read as a positional one.
#[test]
fn named_arguments_bind_by_name_in_declaration_order() {
    // Order-independent: solc reorders to the declaration's order.
    accepted("uint x = f({a: 1, b: 2});");
    accepted("uint x = f({b: 2, a: 1});");
    // The result is the call's, so the named order cannot leak into the type.
    let (db, file) = make_test_db(&NAMED_FIXTURE.replace("@BODY@", "uint x = f({b: 2, a: 1});"));
    let inference = named_function_inference(&db, file, "h");
    assert!(inference.diagnostics.is_empty());
    assert!(
        expr_results(&inference)
            .any(|(_, ty, _)| matches!(ty.kind(&db), TyKind::Primitive(Primitive::Uint(256))))
    );

    let DefId::Function(id) = find_function(&db, file, "h") else {
        panic!("fixture declares h")
    };
    let body = db.body_map(BodyOwnerId::Function(id));
    let group = body
        .syntax_store
        .exprs
        .iter()
        .find_map(|(_, expr)| match expr {
            Expr::NamedArgs(named) => Some(named.clone()),
            _ => None,
        })
        .expect("the fixture has a named group");
    assert_eq!(group.len(), 2);
    let call = body
        .syntax_store
        .exprs
        .iter()
        .find_map(|(_, expr)| match expr {
            Expr::Call { callee, args } if args.len() == 1 => Some(*callee),
            _ => None,
        })
        .expect("the fixture has a call");
    assert!(matches!(body.syntax_store.exprs[call], Expr::Ident(_)));
}

/// A named call reaches every kind whose declaration has parameter names.
#[test]
fn named_arguments_reach_every_named_capable_form() {
    accepted("emit Ev({a: 1, b: 2});");
    accepted("revert Er({a: 1, b: 2});");
    accepted("B b = new B({a: 1});");
    accepted("S memory s = S({x: 1, y: 2});");
    accepted("S memory s = S({y: 2, x: 1});");
    // A positional struct constructor is checked against the same field list.
    accepted("S memory s = S(1, 2);");
}

/// The name is matched against the declaration, so a wrong name, a missing one
/// and a repeated one are three different errors.
#[test]
fn named_argument_errors_match_solc() {
    // solc reports a missing parameter as a plain argument-count error.
    assert!(run("uint x = f({a: 1});").contains(&DiagnosticCode::InvalidArgumentCount));
    assert!(run("uint x = f({});").contains(&DiagnosticCode::InvalidArgumentCount));
    assert!(run("uint x = f({a: 1, z: 2});").contains(&DiagnosticCode::UnknownNamedArgument));
    assert!(run("uint x = f({a: 1, a: 2});").contains(&DiagnosticCode::DuplicateNamedArgument));
    // The argument still converts against the parameter it names.
    assert!(run("uint x = f({a: true, b: 2});").contains(&DiagnosticCode::TypeMismatch));
    // A struct's fields are its parameters.
    assert!(run("S memory s = S({x: 1});").contains(&DiagnosticCode::InvalidArgumentCount));
    assert!(run("S memory s = S({z: 1, y: 2});").contains(&DiagnosticCode::UnknownNamedArgument));
    assert!(run("S memory s = S({x: 1, x: 2});").contains(&DiagnosticCode::DuplicateNamedArgument));
    assert!(run("S memory s = S({x: true, y: 2});").contains(&DiagnosticCode::TypeMismatch));
}

/// Each overload has its own parameter names, so a name selects among them —
/// which is why the permutation cannot happen once, before selection.
#[test]
fn named_arguments_select_an_overload_by_name() {
    accepted("uint x = ov({a: 1});");
    accepted("uint x = ov({b: true});");
    // A name no declaration has is solc's "no matching declaration".
    assert!(run("uint x = ov({z: 1});").contains(&DiagnosticCode::NoMatchingOverload));
    // The name selects the declaration; the argument must still convert.
    assert!(run("uint x = ov({b: 1});").contains(&DiagnosticCode::NoMatchingOverload));
}

/// A callee with no declaration names refuses named arguments, each with solc's
/// own reason: a cast has no parameters, a pointer has none to read, and a
/// variadic builtin has no fixed list.
#[test]
fn named_arguments_are_refused_where_there_are_no_parameter_names() {
    assert!(
        failures("uint8 x = uint8({a: 1});")
            .iter()
            .any(|message| message.contains("type conversion cannot allow named arguments"))
    );
    assert!(
        failures("address payable x = payable({a: 1});")
            .iter()
            .any(|message| message.contains("type conversion cannot allow named arguments"))
    );
    assert!(
        failures("function (uint a, uint b) pure returns (uint) fn; uint x = fn({a: 1, b: 2});")
            .iter()
            .any(|message| message.contains("does not match function declaration"))
    );
    assert!(
        failures("bytes memory b = abi.encode({a: 1});")
            .iter()
            .any(|message| message.contains("arbitrary parameters"))
    );
}

/// A `using`-attached function's receiver is implicit — the member access
/// supplies it — so it is not part of the named set. `token.w({ to: a, v: v })`
/// names the two explicit parameters, not the bound receiver.
#[test]
fn named_arguments_skip_a_bound_receiver() {
    let (db, file) = make_test_db(
        r#"
        interface I { function t(address to) external; }
        library L { function w(I self, address to, uint v) internal {} }
        contract C {
            using L for I;
            function h(I token, address a, uint v) external { token.w({ to: a, v: v }); }
        }
        "#,
    );
    let inference = named_function_inference(&db, file, "h");
    assert!(
        inference.diagnostics.is_empty(),
        "the bound receiver has no key: {:?}",
        codes(&inference)
    );
}

/// `new C(args)` is a call on the constructor reference: it yields the
/// constructed contract, and the constructor's parameters are the call's —
/// arity and each argument's conversion are checked. A contract with no
/// declared constructor has an implicit zero-argument default.
#[test]
fn construction_yields_the_contract_and_checks_the_constructor() {
    let (db, file) = make_test_db(
        r#"
            contract B { constructor(uint a) {} }
            contract NoCtor {}
            contract User {
                function f() external { B b = new B(1); }
                function missing() external { new B(); }
                function extra() external { new B(1, 2); }
                function wrongType() external { new B(true); }
                function noArgs() external { new NoCtor(); }
                function extraDefault() external { new NoCtor(1); }
            }
            "#,
    );

    let inference = named_function_inference(&db, file, "f");
    assert!(inference.diagnostics.is_empty(), "{:?}", codes(&inference));
    assert!(
        expr_results(&inference).any(|(_, ty, resolution)| {
            matches!(ty.kind(&db), TyKind::Contract(_))
                && matches!(
                    resolution,
                    Some(Resolution::Callable(callable))
                        if matches!(callable.origin(), CallableOrigin::Constructor(_))
                )
        }),
        "the call should yield the contract and record its constructor",
    );

    for function in ["missing", "extra", "extraDefault"] {
        let inference = named_function_inference(&db, file, function);
        assert!(
            inference
                .diagnostics
                .iter()
                .any(|d| matches!(d, SemanticDiagnostic::InvalidArgumentCount { .. })),
            "`{function}` should report an arity error: {:?}",
            codes(&inference),
        );
    }
    {
        let function = "wrongType";
        let inference = named_function_inference(&db, file, function);
        assert!(
            inference
                .diagnostics
                .iter()
                .any(|d| matches!(d, SemanticDiagnostic::TypeMismatch { .. })),
            "`{function}` should report a conversion: {:?}",
            codes(&inference),
        );
    }
    let no_args = named_function_inference(&db, file, "noArgs");
    assert!(no_args.diagnostics.is_empty(), "{:?}", codes(&no_args));
}

/// `new` targets must be instantiable contracts; every other declaration kind
/// gets solc's own wording — an interface, a library, a struct, a primitive,
/// or a contract that is still abstract (declared so, or carrying an
/// unimplemented inherited obligation).
#[test]
fn non_instantiable_targets_report_solc_messages() {
    let (db, file) = make_test_db(
        r#"
            interface I { function f() external; }
            library L { function g() internal {} }
            struct S { uint x; }
            abstract contract A { function f() public virtual; }
            contract B is A {}
            contract D is A { function f() public override {} }
            contract User {
                function a() external { new I(); }
                function b() external { new L(); }
                function c() external { new S(); }
                function d() external { new uint(); }
                function e() external { new A(); }
                function g() external { new B(); }
                function concrete() external { new D(); }
            }
            "#,
    );

    for (function, expected) in [
        ("a", "Cannot instantiate an interface."),
        ("b", "Invalid use of a library name."),
        ("c", "Identifier is not a contract."),
        ("d", "Contract or array type expected."),
        ("e", "Cannot instantiate an abstract contract"),
        ("g", "Cannot instantiate an abstract contract"),
    ] {
        let inference = named_function_inference(&db, file, function);
        let messages = failure_messages(&inference);
        assert!(
            messages.iter().any(|message| message.contains(expected)),
            "`{function}` should report {expected:?}: {messages:?}",
        );
    }
    assert!(
        named_function_inference(&db, file, "concrete")
            .diagnostics
            .is_empty(),
    );
}

/// Bare `new C` denotes the constructor itself: a *special function* whose
/// type is the constructor's signature with the contract as its result — a
/// value of function type, which is why `(new B, 1)` is a legal tuple and
/// `g(new B)` reports a *conversion*, but a special function never converts
/// to a function pointer (solc: "Special functions cannot be converted to
/// function types"). `Void` and `Invalid` would both break: `Void` means
/// "denotes no value", so the tuple arm would read `new B` as an empty
/// component, and `Invalid` would swallow the conversion report entirely.
#[test]
fn bare_new_denotes_a_special_function() {
    let (db, file) = make_test_db(
        r#"
            contract B { constructor(uint a) {} }
            contract User {
                function f() external { new B; }
                function inTuple() external { (new B, 1); }
                function asArgument() external { uint x = g(new B); }
                function asPointer() external {
                    function (uint) returns (B) f = new B;
                }
                function g(uint v) internal pure returns (uint) { return v; }
            }
            "#,
    );

    let inference = named_function_inference(&db, file, "f");
    assert!(
        expr_results(&inference).any(|(_, ty, resolution)| {
            matches!(
                ty.kind(&db),
                TyKind::Function {
                    callee: Callee::Constructor(_),
                    ..
                }
            ) && matches!(resolution, Some(Resolution::Callable(_)))
        }),
        "bare `new B` should type as the constructor reference",
    );

    // A function-typed component is a value, so the tuple is fine.
    let tuple = named_function_inference(&db, file, "inTuple");
    assert!(tuple.diagnostics.is_empty(), "{:?}", codes(&tuple));

    // And where it does not fit, the *conversion* is what fails — reported,
    // not swallowed.
    for function in ["asArgument", "asPointer"] {
        let inference = named_function_inference(&db, file, function);
        assert!(
            inference
                .diagnostics
                .iter()
                .any(|d| matches!(d, SemanticDiagnostic::TypeMismatch { .. })),
            "`{function}` should report a conversion: {:?}",
            codes(&inference),
        );
    }
}

/// Call options attach to a contract creation: `new C{salt: …, value: …}(…)`
/// is a creation, so the options gate must not reject it as a non-external
/// callee. `gas` is the one option a creation does not take — solc: "Function
/// call option \"gas\" cannot be used with \"new\"".
#[test]
fn creation_options_are_salt_and_value_only() {
    let (db, file) = make_test_db(
        r#"
            contract B { constructor(uint a) payable {} }
            contract User {
                function ok() external { new B{salt: 0, value: 1}(1); }
                function withGas() external { new B{gas: 1000}(1); }
            }
            "#,
    );

    let ok = named_function_inference(&db, file, "ok");
    assert!(ok.diagnostics.is_empty(), "{:?}", codes(&ok));

    let bad = named_function_inference(&db, file, "withGas");
    assert!(
        bad.diagnostics.iter().any(|d| matches!(
            d,
            SemanticDiagnostic::InferenceFailure { message, .. }
                if message.contains("cannot be used with `new`")
        )),
        "{:?}",
        codes(&bad),
    );
}

/// `new T[](n)` — the second mode of `new`: the target is an *array type* and
/// the single argument is its length. Every case is verified against solc
/// 0.8.34.
const ALLOC_FIXTURE: &str = r#"
    contract A { }
    contract User {
        function h(uint n, uint8 n8) external { @BODY@ }
    }
"#;

fn run_alloc(body: &str) -> Vec<DiagnosticCode> {
    let (db, file) = make_test_db(&ALLOC_FIXTURE.replace("@BODY@", body));
    named_function_inference(&db, file, "h")
        .diagnostics
        .iter()
        .map(SemanticDiagnostic::code)
        .collect()
}

/// Every accepted shape — `new T[](n)` for element types including contracts
/// and nested arrays, and `new bytes(n)`/`new string(n)` (the same mode on a
/// dynamic byte array). `new bytes`/`new string` resolve to an `Allocation`
/// callable whose result is that array at `memory`.
#[test]
fn array_allocation_accepts_the_shapes_solc_does() {
    for body in [
        "uint[] memory a = new uint[](n);",
        "uint[] memory a = new uint[](n8);",
        "uint[] memory a = new uint[](n + 1);",
        "int[] memory a = new int[](n);",
        "A[] memory a = new A[](n);",
        "bytes[] memory a = new bytes[](n);",
        "uint[][] memory a = new uint[][](n);",
        "bytes memory b = new bytes(n);",
        "string memory s = new string(n);",
        "bytes memory b = new bytes(0);",
        "bytes memory b = new bytes(n + 1);",
        "bytes memory b = new bytes(n8);",
    ] {
        let codes = run_alloc(body);
        assert!(
            codes.is_empty(),
            "`{body}` should be accepted, got {codes:?}"
        );
    }

    let (db, file) = make_test_db(&ALLOC_FIXTURE.replace("@BODY@", "bytes memory b = new bytes(n);"));
    let inference = named_function_inference(&db, file, "h");
    assert!(
        expr_results(&inference).any(|(_, ty, resolution)| {
            matches!(
                resolution,
                Some(Resolution::Callable(callable))
                    if matches!(callable.origin(), CallableOrigin::Allocation(_))
            ) && matches!(
                ty.kind(&db),
                TyKind::Ref(inner, _)
                    if matches!(inner.kind(&db), TyKind::Primitive(Primitive::Bytes))
            )
        }),
        "`new bytes(n)` should yield `bytes memory` through an allocation",
    );
}

/// The length is the callable's single parameter, so the ordinary call path
/// checks it: exactly one argument, converting to `uint256`. The result is a
/// fresh `memory` array, so it does not fill a storage reference.
#[test]
fn array_allocation_checks_the_length_like_a_parameter() {
    assert!(
        run_alloc("uint[] memory a = new uint[]();")
            .contains(&DiagnosticCode::InvalidArgumentCount)
    );
    assert!(
        run_alloc("uint[] memory a = new uint[](1, 2);")
            .contains(&DiagnosticCode::InvalidArgumentCount)
    );
    assert!(
        run_alloc("uint[] memory a = new uint[](\"x\");").contains(&DiagnosticCode::TypeMismatch)
    );
    assert!(
        run_alloc("uint[] memory a = new uint[](true);").contains(&DiagnosticCode::TypeMismatch)
    );
    assert!(run_alloc("uint[] storage a = new uint[](n);").contains(&DiagnosticCode::TypeMismatch));
    assert!(
        run_alloc("bytes memory b = new bytes();")
            .contains(&DiagnosticCode::InvalidArgumentCount)
    );
    assert!(
        run_alloc("bytes memory b = new bytes(\"x\");").contains(&DiagnosticCode::TypeMismatch)
    );
    assert!(
        run_alloc("string memory s = new string(true);").contains(&DiagnosticCode::TypeMismatch)
    );
}

/// A fixed size *is* the length, so there is nothing left to pass — solc:
/// "Length has to be placed in parentheses after the array type for new
/// expression." A non-array, non-contract target keeps solc's other message,
/// so the two modes stay distinguishable.
#[test]
fn allocatable_targets_have_the_right_shape() {
    let (db, file) =
        make_test_db(&ALLOC_FIXTURE.replace("@BODY@", "uint[5] memory a = new uint[5]();"));
    let inference = named_function_inference(&db, file, "h");
    assert!(
        inference.diagnostics.iter().any(|d| matches!(
            d,
            SemanticDiagnostic::InferenceFailure { message, .. }
                if message.contains("length has to be placed in parentheses")
        )),
        "{:?}",
        codes(&inference),
    );

    let (db, file) = make_test_db(&ALLOC_FIXTURE.replace("@BODY@", "new uint();"));
    let inference = named_function_inference(&db, file, "h");
    assert!(
        inference.diagnostics.iter().any(|d| matches!(
            d,
            SemanticDiagnostic::InferenceFailure { message, .. }
                if message.contains("Contract or array type expected")
        )),
        "{:?}",
        codes(&inference),
    );
}

/// Bare `new T[]` denotes the allocation itself, which solc types as
/// `function (uint256) pure returns (T[] memory)` — and, like a constructor, it
/// is a *special function*: no pointer conversion and no `.selector`.
#[test]
fn bare_new_array_is_a_special_function() {
    let (db, file) = make_test_db(&ALLOC_FIXTURE.replace("@BODY@", "new uint[];"));
    let inference = named_function_inference(&db, file, "h");
    assert!(inference.diagnostics.is_empty());
    assert!(
        expr_results(&inference).any(|(_, ty, resolution)| {
            matches!(
                ty.kind(&db),
                TyKind::Function {
                    callee: Callee::Allocation(_),
                    ..
                }
            ) && matches!(resolution, Some(Resolution::Callable(_)))
        }),
        "bare `new uint[]` should type as the allocation reference",
    );

    assert!(
        run_alloc("function (uint256) pure returns (uint256[] memory) f = new uint[];")
            .contains(&DiagnosticCode::TypeMismatch)
    );
    assert!(
        run_alloc("bytes4 s = new uint[].selector;").contains(&DiagnosticCode::UnresolvedMember)
    );
}

/// Base constructor arguments in both positions — `contract D is Base(args)`
/// and `constructor() Base(args)` — resolve to `CallableOrigin::Constructor`
/// and check arity and argument conversions against the base's constructor.
#[test]
fn base_constructor_initializations_are_checked_in_both_positions() {
    let (db, file) = make_test_db(
        r#"
            contract A {
                constructor(uint256 a, bool b) {}
            }
            contract OkHeader is A(1, true) {}
            contract BadHeaderArity is A(1) {}
            contract BadHeaderType is A(1, 2) {}
            contract OkCtorMod is A {
                constructor(uint256 x) A(x, false) {}
            }
            contract BadCtorModArity is A {
                constructor() A(1) {}
            }
            contract BadCtorModType is A {
                constructor() A(true, false) {}
            }
            contract NonBase {
                constructor(uint256 x) {}
            }
            contract BadNonBase is A(1, true) {
                constructor() NonBase(1) {}
            }
            contract BadRegularFn is A(1, true) {
                function f() external A(1, true) {}
            }
            "#,
    );

    let contract_inf = |name: &str| db.inference(find_contract(&db, file, name));
    let ctor_inf = |name: &str| {
        let DefId::Contract(id) = find_contract(&db, file, name) else {
            panic!("{name} is a contract");
        };
        let ctor = db
            .contract_data(id)
            .constructor
            .expect("contract has a constructor");
        db.inference(DefId::Function(ctor))
    };

    let ok_header = contract_inf("OkHeader");
    assert!(ok_header.diagnostics.is_empty(), "{:?}", codes(&ok_header));
    assert!(expr_results(&ok_header).any(|(_, _, res)| {
        matches!(
            res,
            Some(Resolution::Callable(c)) if matches!(c.origin(), CallableOrigin::Constructor(_))
        )
    }));

    assert!(
        codes(&contract_inf("BadHeaderArity")).contains(&DiagnosticCode::InvalidArgumentCount)
    );
    assert!(codes(&contract_inf("BadHeaderType")).contains(&DiagnosticCode::TypeMismatch));

    let ok_ctor = ctor_inf("OkCtorMod");
    assert!(ok_ctor.diagnostics.is_empty(), "{:?}", codes(&ok_ctor));
    assert!(expr_results(&ok_ctor).any(|(_, _, res)| {
        matches!(
            res,
            Some(Resolution::Callable(c)) if matches!(c.origin(), CallableOrigin::Constructor(_))
        )
    }));

    assert!(codes(&ctor_inf("BadCtorModArity")).contains(&DiagnosticCode::InvalidArgumentCount));
    assert!(codes(&ctor_inf("BadCtorModType")).contains(&DiagnosticCode::TypeMismatch));

    // A non-base contract or a regular function may not invoke a base constructor.
    assert!(
        failure_messages(&ctor_inf("BadNonBase"))
            .iter()
            .any(|m| m.contains("not a modifier"))
    );
    assert!(
        failure_messages(&named_function_inference(&db, file, "f"))
            .iter()
            .any(|m| m.contains("not a modifier"))
    );
}

/// `is Base(args)` arguments resolve in the inheriting contract's scope —
/// its own members and inherited names are visible, not just the file's.
#[test]
fn specifier_base_constructor_args_see_the_contract_scope() {
    let (db, file) = make_test_db(
        r#"
            contract A {
                uint256 constant K = 1;
                constructor(uint256) {}
            }
            contract OwnMember is A(K) { uint256 constant K = 2; }
            contract Inherited is A(K) {}
            contract Missing is A(NOPE) {}
            "#,
    );

    let contract_inf = |name: &str| db.inference(find_contract(&db, file, name));

    let own = contract_inf("OwnMember");
    assert!(own.diagnostics.is_empty(), "{:?}", codes(&own));
    let inherited = contract_inf("Inherited");
    assert!(inherited.diagnostics.is_empty(), "{:?}", codes(&inherited));
    assert!(contract_inf("Missing").diagnostics.iter().any(|d| {
        matches!(d, SemanticDiagnostic::UnresolvedName { name, .. } if name.as_str() == "NOPE")
    }));
}

/// An unresolvable base reports `UnresolvedName` once — the specifier lowers
/// its base path once as an expression, so `is Missing(1)` diagnoses
/// identically to a bare `is Missing`.
#[test]
fn specifier_base_constructor_reports_one_name_diagnostic() {
    let (db, file) = make_test_db(
        r#"
            contract Bare is Missing {}
            contract Argd is Missing(1) {}
        "#,
    );

    let contract_inf = |name: &str| db.inference(find_contract(&db, file, name));

    for name in ["Bare", "Argd"] {
        let inf = contract_inf(name);
        assert_eq!(
            inf.diagnostics
                .iter()
                .filter(|d| matches!(d, SemanticDiagnostic::UnresolvedName { .. }))
                .count(),
            1,
            "{name}: {:?}",
            codes(&inf)
        );
        assert_eq!(inf.diagnostics.len(), 1, "{name}: {:?}", codes(&inf));
    }
}
