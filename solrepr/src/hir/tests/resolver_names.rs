use super::{helper::*, resolver_helpers::*};
use crate::hir::semantic_errors::SemanticDiagnostic;
use crate::ir::def_map::DefId;
use crate::salsa::HirDb;
use crate::hir::resolver::*;
use crate::hir::resolution::*;
use crate::hir::types::{BaseKind, CallKind, Ty, TyKind};
use crate::hir::linearize::Linearizer;

/// The declaration-phase constructor never loads its container's body map,
/// so signature inference cannot depend on body edits through the resolver.
#[test]
fn declaration_resolver_does_not_load_the_body_map() {
    let (db, file) = make_test_db(r#"contract C { function f() external { uint x = 1; } }"#);
    let f = find_function(&db, file, "f");
    assert!(Resolver::build(&db, f).body().is_some());
    assert!(Resolver::for_declaration(&db, f).body().is_none());
}

#[test]
fn type_name_resolution_splits_types_from_modules() {
    let (db, file) = make_test_db(
        r#"
        contract C {
            function f() external {}
        }
        "#,
    );

    // A file def is a module namespace, not a type.
    assert!(matches!(
        Resolution::from_type(&db, DefId::File(file)),
        Resolution::Module(module) if module == file
    ));

    // A declared type is `Type`, and always nameable.
    let Resolution::Type(ty) = Resolution::from_type(&db, find_contract(&db, file, "C")) else {
        panic!("a contract def should resolve to a type name");
    };
    assert!(matches!(ty.kind(&db), TyKind::Contract(_)));
}

/// A file module is a *namespace*, not a type or a receiver. A free function
/// reached through it (`M.f`) keeps its natural call kind — free functions
/// are always `internal`, so the reference is a direct/internal jump, never
/// a declaration-only `ViaType`. This pins that the module member path goes
/// through `lookup_file`, not the type-namespace retag in `types_member`.

#[test]
fn module_namespace_member_keeps_its_natural_call_kind() {
    let (db, file) = make_test_db(
        r#"
        function free() pure returns (uint) { return 1; }
        contract C { function f() external {} }
        "#,
    );
    let resolver = Resolver::build(
        &db,
        find_function(&db, file, "f"),
    );
    let module = Ty::new(&db, TyKind::Module(file));
    let Some(Resolution::Callable(callable)) = lookup_member(&resolver, module, &"free".into())
    else {
        panic!("a file-scope free function should resolve through the module namespace");
    };
    assert!(matches!(callable.kind(), CallKind::Direct));
    assert!(callable.is_invocable(&db));
}

/// The tag on a reference reached through a type name is the *access path*;
/// whether it may be invoked is a separate question, decided by the
/// declaration kind and the owner together. Verified against solc 0.8.33: a
/// function through an *unrelated* contract name is declaration-only ("Cannot
/// call function via contract type name"), while a self-qualified `C.fn`
/// inside `C` is an ordinary internal call; a modifier through a library name
/// is rejected, and errors and events are invocable through any namespace.

#[test]
fn type_namespace_reference_tag_and_invocability_match_solc() {
    let (db, file) = make_test_db(
        r#"
        library L {
            function fn() internal pure {}
            modifier mod() { _; }
            error LErr(uint x);
            event LEv(uint x);
        }
        contract X {
            function fn() public {}
            modifier mod() { _; }
            error CErr(uint x);
            event CEv(uint x);
        }
        contract C {
            function fn() public {}
            function probe() external {}
        }
        "#,
    );
    let resolver = Resolver::build(
        &db,
        find_function(&db, file, "probe"),
    );
    let ty = |name: &str| match resolver.resolve_name(&name.into(), None, 0) {
        Some(Resolution::Type(ty)) => ty.ty(),
        _ => panic!("{name} should resolve to a type"),
    };
    let callable = |owner: Ty, name: &str| match lookup_member(&resolver, owner, &name.into()) {
        Some(Resolution::Callable(c)) => c,
        _ => panic!("{name} should resolve to a callable"),
    };
    let lib = types_receiver(&db, ty("L"));
    let contract = types_receiver(&db, ty("X"));
    let this_contract = types_receiver(&db, ty("C"));

    // The tag records the access path, whatever the member is.
    for (owner, member) in [
        (lib, "fn"),
        (lib, "mod"),
        (lib, "LErr"),
        (lib, "LEv"),
        (contract, "fn"),
        (contract, "mod"),
        (contract, "CErr"),
        (contract, "CEv"),
    ] {
        assert!(
            matches!(callable(owner, member).kind(), CallKind::ViaType(_)),
            "`{member}` should be tagged by its access path",
        );
    }

    // Self-qualified and ancestor-qualified declarations retain a base-access
    // tag; invocation behavior is derived from this tag and declaration facts.
    assert!(
        matches!(
            callable(this_contract, "fn").kind(),
            CallKind::Base(BaseKind::SelfQualified)
        ),
        "self-qualified `C.fn` retains its type-qualified route",
    );

    assert!(
        callable(lib, "fn").is_invocable(&db),
        "L.fn() is a delegatecall"
    );
    assert!(
        !callable(contract, "fn").is_invocable(&db),
        "X.fn() is declaration-only"
    );
    assert!(
        callable(this_contract, "fn").is_invocable(&db),
        "C.fn() inside C is a self-qualified internal call"
    );
    assert!(
        !callable(lib, "mod").is_invocable(&db),
        "a library's modifiers are not reachable from outside"
    );
    assert!(
        callable(contract, "mod").is_invocable(&db),
        "X.mod is the normal way to name a modifier"
    );
    assert!(callable(lib, "LErr").is_invocable(&db), "revert L.LErr(..)");
    assert!(
        callable(contract, "CErr").is_invocable(&db),
        "revert C.CErr(..)"
    );
    assert!(callable(lib, "LEv").is_invocable(&db), "emit L.LEv(..)");
    assert!(
        callable(contract, "CEv").is_invocable(&db),
        "emit C.CEv(..)"
    );
}

/// The C3 pass reports an `InheritanceFailure` for an unresolved base and
/// for a cyclic one.
#[test]
fn broken_bases_report_inheritance_failures() {
    let (db, file) = make_test_db("contract Child is MissingBase {}");
    let child = find_contract(&db, file, "Child");
    let analysis = Linearizer::new(&db, child).c3_linearize();
    assert!(analysis.diagnostics.iter().any(|diagnostic| {
        matches!(diagnostic, SemanticDiagnostic::InheritanceFailure { problems, .. } if problems.iter().any(|problem| problem.contains("unresolved base")))
    }));

    let (db, file) = make_test_db("contract A is A {}");
    let a = find_contract(&db, file, "A");
    let analysis = Linearizer::new(&db, a).c3_linearize();
    assert!(analysis.diagnostics.iter().any(|diagnostic| {
        matches!(diagnostic, SemanticDiagnostic::InheritanceFailure { problems, .. } if problems.iter().any(|problem| problem.contains("cyclic inheritance")))
    }));
}

/// A `using` candidate attaches only when its first parameter accepts the
/// receiver: `ext(uint256 self)` is not a member of `C`, and `IERC20` — a
/// *supertype* of `IERC1363` — does not reach `IERC1363`'s extension.
#[test]
fn using_candidates_are_filtered_by_receiver_convertibility() {
    let (db, file) = make_test_db(
        r#"
        library Ext {
            function accepts(C self) internal pure returns (uint256) {}
            function rejects(uint256 self) internal pure returns (uint256) {}
        }
        contract C {
            using Ext for C;
            function f() external {}
        }
        "#,
    );
    let f = find_function(&db, file, "f");
    let resolver = Resolver::build(&db, f);
    let c = contract_type(&db, file, "C");

    let Resolution::Callable(callable) = lookup_member(&resolver, c, &"accepts".into()).unwrap()
    else {
        panic!("accepts should resolve to a callable");
    };
    assert_eq!(callable.bound(), 1);
    assert!(lookup_member(&resolver, c, &"rejects".into()).is_none());

    let (db, file) = make_test_db(
        r#"
        interface IERC20 {}
        interface IERC1363 is IERC20 {}
        library SafeERC20 {
            function safeTransfer(IERC20 token, address to, uint256 value) internal {}
            function transferAndCallRelaxed(IERC1363 token, address to, uint256 value, bytes memory data) internal {}
        }
        contract C {
            using SafeERC20 for IERC20;
            function f() external {}
        }
        "#,
    );
    let f = find_function(&db, file, "f");
    let resolver = Resolver::build(&db, f);
    let Resolution::Type(ierc20) = resolver.resolve_name(&"IERC20".into(), None, 0).unwrap() else {
        panic!("IERC20 should resolve to a type");
    };

    let members = member_resolutions(&resolver, ierc20.ty());
    let names: Vec<String> = members
        .iter()
        .filter_map(|member| match member {
            Resolution::Callable(callable) => match callable.def() {
                Some(DefId::Function(id)) => Some(db.function_data(id).name.to_string()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert!(names.iter().any(|n| n == "safeTransfer"), "got {names:?}");
    assert!(
        !names.iter().any(|n| n == "transferAndCallRelaxed"),
        "got {names:?}"
    );
}
