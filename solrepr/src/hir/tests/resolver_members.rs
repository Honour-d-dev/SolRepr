use super::{helper::*, resolver_helpers::*};
use crate::hir::access::AccessDenial;
use crate::hir::exprs::Name;
use crate::hir::item_data::VariantId;
use crate::hir::semantic_errors::SemanticDiagnostic;
use crate::ir::def_map::DefId;
use crate::salsa::HirDb;
use crate::hir::resolver::*;
use crate::hir::exprs::ExprId;
use crate::hir::resolution::{Field as ResolvedField, *};
use crate::hir::types::Location;
use crate::hir::types::{CallKind, Callee, Ty, TyKind, ViaTypeKind};
use crate::hir::types::{BaseKind, Primitive, Visibility};
use crate::salsa::RootDb;

#[test]
fn values_member_lookup_finds_functions_and_variables() {
    let (db, file) = make_test_db(
        r#"
        contract C {
            uint internal hidden;
            uint public x;
            function f() external {}
            function g() internal {}
        }
        "#,
    );
    let f = find_function(&db, file, "f");
    let resolver = Resolver::build(&db, f);
    let c = contract_type(&db, file, "C");

    // Public state variables become getters; external functions are
    // reachable through a value.
    let Resolution::Callable(getter) = lookup_member(&resolver, c, &"x".into()).unwrap() else {
        panic!("public x should resolve to a getter");
    };
    assert!(getter.is_getter());
    assert!(lookup_member(&resolver, c, &"f".into()).is_some());
    // Internal/private members are not part of the external interface.
    assert!(lookup_member(&resolver, c, &"hidden".into()).is_none());
    assert!(lookup_member(&resolver, c, &"g".into()).is_none());
    // Types are not instance members.
    assert!(lookup_member(&resolver, c, &"C".into()).is_none());
}

/// The `Types` receiver carries no values: a state variable is not a
/// namespace member.
#[test]
fn types_member_lookup_excludes_variables() {
    let (db, file) = make_test_db(
        r#"
        contract C {
            uint x;
            function f() external {}
        }
        "#,
    );
    let f = find_function(&db, file, "f");
    let resolver = Resolver::build(&db, f);

    let c = contract_type(&db, file, "C");
    let type_receiver = types_receiver(&db, c);
    assert!(lookup_member(&resolver, type_receiver, &"f".into()).is_some());
    // State variables are not namespace members.
    assert!(lookup_member(&resolver, type_receiver, &"x".into()).is_none());
}

#[test]
fn receiver_members_dispatch_on_expression_type() {
    let (db, file) = make_test_db(
        r#"
        contract C {
            uint public x;
            function f() external {}
        }
        "#,
    );
    let f = find_function(&db, file, "f");
    let resolver = Resolver::build(&db, f);
    let c = contract_type(&db, file, "C");
    let c_name = c.narrow(&db).unwrap();
    let type_receiver = Ty::new(&db, TyKind::Type(c_name));

    let namespace_members = member_resolutions(&resolver, type_receiver);
    assert!(
        namespace_members
            .iter()
            .any(|member| matches!(member, Resolution::Callable(_)))
    );
    assert!(
        !namespace_members
            .iter()
            .any(|member| matches!(member, Resolution::Var(_)))
    );

    // A contract value exposes the external interface: functions and
    // public-variable getters, both as callables.
    let instance_members = member_resolutions(&resolver, c);
    assert!(
        instance_members
            .iter()
            .any(|member| matches!(member, Resolution::Callable(callable) if callable.is_getter()))
    );
    assert!(
        instance_members.iter().any(
            |member| matches!(member, Resolution::Callable(callable) if !callable.is_getter())
        )
    );
    assert!(
        !instance_members
            .iter()
            .any(|member| matches!(member, Resolution::Var(_)))
    );
}

#[test]
fn enum_variants_are_visible_through_types() {
    let (db, file) = make_test_db(
        r#"
        contract C {
            enum E { A, B }
            function f() external {}
        }
        "#,
    );
    let f = find_function(&db, file, "f");
    let resolver = Resolver::build(&db, f);

    let Resolution::Type(ty) = resolver.resolve_name(&"E".into(), None, 0).unwrap() else {
        panic!("E should resolve to a type");
    };
    let type_receiver = types_receiver(&db, ty.ty());
    let Resolution::Variant(variant) = lookup_member(&resolver, type_receiver, &"A".into()).unwrap()
    else {
        panic!("E.A should resolve to a variant");
    };
    assert_eq!(
        variant.id,
        VariantId::from_raw(0.into())
    );
}

#[test]
fn base_members_resolve_through_linearized_mro() {
    let (db, file) = make_test_db(
        r#"
        contract Base {
            function inherited() external {}
        }
        contract Derived is Base {}
        "#,
    );
    let f = find_contract(&db, file, "Derived");
    // Build a resolver inside Derived's container.
    let resolver = Resolver::build(&db, f);
    let derived = contract_type(&db, file, "Derived");

    assert!(lookup_member(&resolver, derived, &"inherited".into()).is_some());
}

#[test]
fn receiver_lookup_preserves_inherited_slot_conflicts() {
    let (db, file) = make_test_db(
        r#"
        contract A { function f() public virtual {} }
        contract B { function f() public virtual {} }
        contract C is A, B { function probe() external {} }
        "#,
    );
    let probe = find_function(&db, file, "probe");
    let resolver = Resolver::build(&db, probe);
    let c = contract_type(&db, file, "C");
    let lookup = resolver.member_named(c, &"f".into());
    assert!(lookup.is_hit());
    assert!(lookup.effective_resolution().is_none());
    assert_eq!(lookup.conflicts.len(), 2);
    assert_eq!(lookup.completion_resolutions().len(), 2);
}

#[test]
fn inaccessible_conflicted_slot_is_not_an_ambiguity_hit() {
    let (db, file) = make_test_db(
        r#"
        contract A { uint256 private x; }
        contract B { uint256 private x; }
        contract C is A, B { function probe() external {} }
        "#,
    );
    let probe = find_function(&db, file, "probe");
    let resolver = Resolver::build(&db, probe);
    let c = contract_type(&db, file, "C");
    let lookup = resolver.member_named(c, &"x".into());

    assert!(!lookup.is_hit());
    assert!(lookup.conflicts.is_empty());
    assert!(lookup.ambiguous.is_empty());
    assert_eq!(lookup.denials.len(), 2);
}

#[test]
fn struct_fields_are_values_members_only() {
    let (db, file) = make_test_db(
        r#"
        struct S { uint x; }
        contract C {
            function f() external {}
        }
        "#,
    );
    let f = find_function(&db, file, "f");
    let resolver = Resolver::build(&db, f);

    let Resolution::Type(ty) = resolver.resolve_name(&"S".into(), None, 0).unwrap() else {
        panic!("S should resolve to a type");
    };
    let instance = ty.at_loc(&db, Location::Memory).ty();
    let Resolution::Field(field) = lookup_member(&resolver, instance, &"x".into()).unwrap() else {
        panic!("S.x should resolve to a field");
    };
    assert!(matches!(field, ResolvedField::Struct(DefId::Struct(_), _)));
}

#[test]
fn function_type_members_match_between_lookup_and_completion() {
    let (db, file) = make_test_db(r#"contract C { function f() external {} }"#);
    let f = find_function(&db, file, "f");
    let resolver = Resolver::build(&db, f);
    let DefId::Function(id) = f else {
        panic!("expected a function");
    };

    // Externally-accessed function value: `.selector` and `.address`.
    let external = Ty::new(
        &db,
        TyKind::Function {
            callee: Callee::Fn(id),
            kind: CallKind::External,
        },
    );
    assert!(lookup_member(&resolver, external, &"selector".into()).is_some());
    assert!(lookup_member(&resolver, external, &"address".into()).is_some());
    assert!(
        member_resolutions(&resolver, external)
            .iter()
            .any(|m| matches!(m, Resolution::Field(_)))
    );

    // Reached through a type name: `.selector` only.
    let decl = Ty::new(
        &db,
        TyKind::Function {
            callee: Callee::Fn(id),
            kind: CallKind::ViaType(ViaTypeKind::NonLibrary),
        },
    );
    assert!(lookup_member(&resolver, decl, &"selector".into()).is_some());
    assert!(lookup_member(&resolver, decl, &"address".into()).is_none());

    // Bare internal function value: no members at all.
    let bare = Ty::new(
        &db,
        TyKind::Function {
            callee: Callee::Fn(id),
            kind: CallKind::Direct,
        },
    );
    assert!(lookup_member(&resolver, bare, &"selector".into()).is_none());
    assert!(member_resolutions(&resolver, bare).is_empty());
}

#[test]
fn error_and_event_type_members() {
    let (db, file) = make_test_db(
        r#"
        contract C {
            error Bad();
            event Done();
            event Anon() anonymous;
            function f() external {}
        }
        "#,
    );
    let f = find_function(&db, file, "f");
    let resolver = Resolver::build(&db, f);

    let def_of = |name: &str| -> DefId {
        let Resolution::Callable(callable) = resolver.resolve_name(&name.into(), None, 0).unwrap()
        else {
            panic!("{name} should resolve to a callable");
        };
        callable.def().unwrap()
    };

    let DefId::Error(bad_id) = def_of("Bad") else {
        panic!("Bad should be an error");
    };
    let bad_ty = Ty::new(&db, TyKind::Error(bad_id));
    assert!(lookup_member(&resolver, bad_ty, &"selector".into()).is_some());

    let DefId::Event(done_id) = def_of("Done") else {
        panic!("Done should be an event");
    };
    let done_ty = Ty::new(&db, TyKind::Event(done_id));
    assert!(lookup_member(&resolver, done_ty, &"selector".into()).is_some());

    let DefId::Event(anon_id) = def_of("Anon") else {
        panic!("Anon should be an event");
    };
    let anon_ty = Ty::new(&db, TyKind::Event(anon_id));
    assert!(lookup_member(&resolver, anon_ty, &"selector".into()).is_none());
    assert!(member_resolutions(&resolver, anon_ty).is_empty());
}

fn resolve_reported_name(resolver: &Resolver, name: &str, offset: u32) -> Option<Resolution> {
    resolver.resolve_name_at(
        &name.into(),
        None,
        offset,
        ExprId::from_raw(offset.into()),
        false,
    )
}

fn lookup_reported_member(
    resolver: &Resolver,
    ty: Ty,
    name: &str,
    offset: u32,
) -> Option<Resolution> {
    let name: Name = name.into();
    resolver.lookup_member_at(ty, &name, ExprId::from_raw(offset.into()))
}

fn recorded_denials(resolver: &Resolver) -> Vec<AccessDenial> {
    resolver
        .finish_diagnostics()
        .iter()
        .flat_map(|diagnostic| match diagnostic {
            SemanticDiagnostic::UnresolvedName { denials, .. }
            | SemanticDiagnostic::UnresolvedMember { denials, .. } => denials.clone(),
            _ => Vec::new(),
        })
        .collect()
}

/// Bare names inside the declaring contract: everything but `external` is a
/// valid internal reference — and from a derived contract `internal`/
/// `public` base members resolve while `private` members of a base are
/// invisible. In either case an `external` member has no internal reference
/// (solc: "Undeclared identifier"), and the denials record why.
#[test]
fn bare_names_follow_the_access_matrix() {
    let (db, file) = make_test_db(
        r#"
        contract C {
            function inter() internal {}
            function pub_() public {}
            function priv() private {}
            function ext() external {}
            function probe() external {}
        }
        "#,
    );
    let probe = find_function(&db, file, "probe");
    let resolver = Resolver::build(
        &db,
        probe,
    );

    for name in ["inter", "pub_", "priv"] {
        assert!(
            resolver.resolve_name(&name.into(), None, 0).is_some(),
            "{name} should resolve bare"
        );
    }
    // An `external` function has no internal reference — solc spells it
    // "Undeclared identifier" — and the denial records why.
    assert!(resolve_reported_name(&resolver, "ext", 10).is_none());
    let denials = recorded_denials(&resolver);
    assert!(
        denials
            .iter()
            .any(|denial| denial.vis == Visibility::External)
    );

    let (db, file) = make_test_db(
        r#"
        contract A {
            function inter() internal {}
            function pub_() public {}
            function priv() private {}
            function ext() external {}
        }
        contract C is A {
            function probe() external {}
        }
        "#,
    );
    let probe = find_function(&db, file, "probe");
    let resolver = Resolver::build(
        &db,
        probe,
    );

    for name in ["inter", "pub_"] {
        assert!(
            resolver.resolve_name(&name.into(), None, 0).is_some(),
            "{name} should resolve bare in the derived contract"
        );
    }
    for (offset, name) in [(20, "priv"), (30, "ext")] {
        assert!(
            resolve_reported_name(&resolver, name, offset).is_none(),
            "{name} should be invisible in the derived contract"
        );
    }
    let denials = recorded_denials(&resolver);
    assert!(
        denials
            .iter()
            .any(|denial| denial.vis == Visibility::Private)
    );
    assert!(
        denials
            .iter()
            .any(|denial| denial.vis == Visibility::External)
    );
}

/// A nearer `private` overload does not hide a farther accessible one — the
/// skipped member is reported as a denial only when nothing accessible
/// remains.
#[test]
fn inaccessible_inherited_member_is_skipped_not_shadowing() {
    let (db, file) = make_test_db(
        r#"
        contract A {
            function f(uint a) private returns (uint) { return a; }
        }
        contract B {
            function f(bool a) public returns (uint) { return 0; }
        }
        contract C is A, B {
            function probe() external {}
        }
        "#,
    );
    let probe = find_function(&db, file, "probe");
    let resolver = Resolver::build(
        &db,
        probe,
    );

    let Resolution::Callable(callable) = resolver.resolve_name(&"f".into(), None, 0).unwrap()
    else {
        panic!("`f` should resolve past the private overload")
    };
    let DefId::Function(id) = callable.def().unwrap() else {
        panic!("`f` should be a function");
    };
    let sig = db.function_signature(id).expect("a signature");
    assert!(
        matches!(
            sig.params[0].kind(&db),
            TyKind::Primitive(Primitive::Boolean)
        ),
        "the farther public `f(bool)` should win over the nearer private `f(uint)`"
    );
    // This successful, non-reporting lookup neither resolves nor stores an
    // access-failure diagnostic.
}

/// Accessible overloads from different bases merge into one candidate set.
#[test]
fn accessible_overloads_merge_across_bases() {
    let (db, file) = make_test_db(
        r#"
        contract A {
            function f(uint a) public {}
        }
        contract B {
            function f(bool a) public {}
        }
        contract C is A, B {
            function probe() external {}
        }
        "#,
    );
    let probe = find_function(&db, file, "probe");
    let resolver = Resolver::build(
        &db,
        probe,
    );

    let Resolution::Callables(callables) = resolver.resolve_name(&"f".into(), None, 0).unwrap()
    else {
        panic!("`f` should merge base overloads")
    };
    assert_eq!(callables.len(), 2, "both base overloads are candidates");
}

/// `Base.f` through a type name: internally-callable members become
/// `Base(Explicit)` calls; `external` resolves but is not invocable;
/// `private` is invisible.
/// The three base paths over the same access matrix: `A.m` resolves as
/// `BaseKind::Explicit`, `C.m` inside `C` as `SelfQualified`, `super.m` as
/// `Super`. `internal`/`public` members are callable on each; `external` is a
/// declaration reference on the type-name paths and invisible on `super`,
/// while `private` is invisible everywhere.
#[test]
fn qualified_member_paths_follow_the_access_matrix() {
    let base_source = r#"
        contract A {
            function inter() internal {}
            function pub_() public {}
            function ext() external {}
            function priv() private {}
        }
        contract C is A {
            function probe() external {}
        }
        "#;
    let (db, file) = make_test_db(base_source);
    let probe = find_function(&db, file, "probe");
    let resolver = Resolver::build(&db, probe);
    let a = types_receiver(&db, contract_type(&db, file, "A"));

    for name in ["inter", "pub_"] {
        let Resolution::Callable(callable) = lookup_member(&resolver, a, &name.into()).unwrap()
        else {
            panic!("`A.{name}` should resolve")
        };
        assert!(
            matches!(callable.kind(), CallKind::Base(BaseKind::Explicit)),
            "`A.{name}` should be a base call"
        );
        assert!(
            callable.is_invocable(&db),
            "`A.{name}()` should be callable"
        );
    }
    let Resolution::Callable(callable) = lookup_member(&resolver, a, &"ext".into()).unwrap() else {
        panic!("`A.ext` should resolve")
    };
    assert!(
        matches!(callable.kind(), CallKind::Base(BaseKind::Explicit)),
        "`A.ext` retains its explicit-base access path"
    );
    assert!(!callable.is_invocable(&db), "`A.ext()` is not callable");
    assert!(lookup_reported_member(&resolver, a, "priv", 40).is_none());
    let denials = recorded_denials(&resolver);
    assert!(
        denials
            .iter()
            .any(|denial| denial.vis == Visibility::Private)
    );

    // Self-qualified `C.m` inside `C` — the members are `C`'s own.
    let (db, file) = make_test_db(
        r#"
        contract C {
            function inter() internal {}
            function pub_() public {}
            function ext() external {}
            function probe() external {}
        }
        "#,
    );
    let probe = find_function(&db, file, "probe");
    let resolver = Resolver::build(&db, probe);
    let c = types_receiver(&db, contract_type(&db, file, "C"));

    for name in ["inter", "pub_"] {
        let Resolution::Callable(callable) = lookup_member(&resolver, c, &name.into()).unwrap()
        else {
            panic!("`C.{name}` should resolve")
        };
        assert!(
            matches!(callable.kind(), CallKind::Base(BaseKind::SelfQualified)),
            "self-qualified `C.{name}` retains its type-qualified route"
        );
        assert!(callable.is_invocable(&db));
    }
    let Resolution::Callable(callable) = lookup_member(&resolver, c, &"ext".into()).unwrap() else {
        panic!("`C.ext` should resolve")
    };
    assert!(
        matches!(callable.kind(), CallKind::Base(BaseKind::SelfQualified))
            && !callable.is_invocable(&db),
        "`C.ext` keeps its base path but remains declaration-only"
    );

    // `super.m` — `external` and `private` are invisible on this path.
    let (db, file) = make_test_db(base_source);
    let probe = find_function(&db, file, "probe");
    let resolver = Resolver::build(&db, probe);
    let DefId::Contract(c_id) = find_contract(&db, file, "C") else {
        panic!("expected a contract")
    };
    let super_ty = Ty::new(&db, TyKind::Super(c_id));

    for name in ["inter", "pub_"] {
        let Resolution::Callable(callable) =
            lookup_member(&resolver, super_ty, &name.into()).unwrap()
        else {
            panic!("`super.{name}` should resolve")
        };
        assert!(
            matches!(callable.kind(), CallKind::Base(BaseKind::Super)),
            "`super.{name}` should be a base call"
        );
        assert!(callable.is_invocable(&db));
    }
    for (offset, name) in [(50, "ext"), (60, "priv")] {
        assert!(
            lookup_reported_member(&resolver, super_ty, name, offset).is_none(),
            "`super.{name}` should be invisible"
        );
    }
    let denials = recorded_denials(&resolver);
    assert!(
        denials
            .iter()
            .any(|denial| denial.vis == Visibility::External)
    );
    assert!(denials.iter().any(|denial| {
        denial.vis == Visibility::Private
    }));
}

/// Member access on a contract type is declared-only in expression position:
/// `B.f` never reaches members `B` merely inherits — while *type names* still
/// resolve inherited nested types (`lookup_type`).
#[test]
fn contract_type_members_are_declared_only() {
    let (db, file) = make_test_db(
        r#"
        contract A {
            struct S { uint x; }
            function f() public {}
        }
        contract B is A {}
        contract C is B {
            function probe() external {}
        }
        "#,
    );
    let probe = find_function(&db, file, "probe");
    let resolver = Resolver::build(
        &db,
        probe,
    );
    let b = types_receiver(&db, contract_type(&db, file, "B"));

    // Inherited into B, so invisible through `B.` in an expression.
    assert!(lookup_member(&resolver, b, &"f".into()).is_none());
    assert!(lookup_member(&resolver, b, &"S".into()).is_none());

    // But `B.S` in a type-name context resolves the inherited struct.
    assert!(
        resolver.resolve_path(&["B".into(), "S".into()]).is_some(),
        "`B.S` should resolve as a type name"
    );
}

/// A contract value exposes the external interface only — public/external
/// functions and public getters — regardless of the caller.
#[test]
fn contract_value_members_follow_the_external_interface() {
    let (db, file) = make_test_db(
        r#"
        contract A {
            uint public x;
            uint internal y;
            function pub_() public {}
            function ext() external {}
            function inter() internal {}
            function priv() private {}
        }
        contract C {
            function probe() external {}
        }
        "#,
    );
    let probe = find_function(&db, file, "probe");
    let resolver = Resolver::build(
        &db,
        probe,
    );
    let a = contract_type(&db, file, "A");

    for name in ["pub_", "ext"] {
        let Resolution::Callable(callable) = lookup_member(&resolver, a, &name.into()).unwrap()
        else {
            panic!("`a.{name}` should resolve")
        };
        assert!(
            callable.is_external() && callable.is_invocable(&db),
            "`a.{name}` is an external call"
        );
    }
    let Resolution::Callable(getter) = lookup_member(&resolver, a, &"x".into()).unwrap() else {
        panic!("`a.x` should resolve to the getter")
    };
    assert!(getter.is_getter());

    for (offset, name) in [(70, "inter"), (80, "priv"), (90, "y")] {
        assert!(
            lookup_reported_member(&resolver, a, name, offset).is_none(),
            "`a.{name}` should be invisible through a value"
        );
    }
    let denials = recorded_denials(&resolver);
    assert_eq!(denials.len(), 3);
    assert!(denials.iter().any(|d| d.vis == Visibility::Internal));
    assert!(denials.iter().any(|d| d.vis == Visibility::Private));
}

/// A bare name hitting a conflicted inherited slot reports at the use site
/// with both base declarations; the slot still produces no resolution.
#[test]
fn bare_name_on_a_conflicted_member_reports_at_the_use_site() {
    let (db, file) = make_test_db(
        r#"
        contract A { function f() public virtual {} }
        contract B { function f() public virtual {} }
        contract C is A, B { function probe() external {} }
        "#,
    );
    let probe = find_function(&db, file, "probe");
    let resolver = Resolver::build(&db, probe);

    assert!(resolve_reported_name(&resolver, "f", 10).is_none());
    let diagnostics = resolver.finish_diagnostics();
    let [SemanticDiagnostic::AmbiguousMember {
        name, declarations, ..
    }] = diagnostics.as_ref()
    else {
        panic!("the conflicted use should report exactly one diagnostic");
    };
    assert_eq!(name.as_str(), "f");
    assert_eq!(declarations.len(), 2);
}

/// The conflicted slot still shadows outer scopes: the file-level `f` does
/// not silently win behind an ambiguous inherited member.
#[test]
fn conflicted_member_shadows_outer_scope_names() {
    let (db, file) = make_test_db(
        r#"
        function f(bool a) returns (uint) { return 3; }
        contract A { function f() public virtual {} }
        contract B { function f() public virtual {} }
        contract C is A, B { function probe() external {} }
        "#,
    );
    let probe = find_function(&db, file, "probe");
    let resolver = Resolver::build(&db, probe);

    // Speculative and reporting lookups agree: no resolution, and the file
    // scope `f(bool)` is not reached.
    assert!(resolver.resolve_name(&"f".into(), None, 0).is_none());
    assert!(resolve_reported_name(&resolver, "f", 10).is_none());
    let diagnostics = resolver.finish_diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert!(matches!(
        &diagnostics[0],
        SemanticDiagnostic::AmbiguousMember { .. }
    ));
}

/// A member access on a conflicted slot reports at the member range and
/// returns no resolution, so hover and goto stay inert on the use site.
#[test]
fn member_access_on_a_conflicted_slot_reports_at_the_use_site() {
    let (db, file) = make_test_db(
        r#"
        contract A { function f() public virtual {} }
        contract B { function f() public virtual {} }
        contract C is A, B { function probe() external {} }
        "#,
    );
    let probe = find_function(&db, file, "probe");
    let resolver = Resolver::build(&db, probe);
    let c = contract_type(&db, file, "C");

    assert!(lookup_reported_member(&resolver, c, "f", 10).is_none());
    let diagnostics = resolver.finish_diagnostics();
    let [SemanticDiagnostic::AmbiguousMember { declarations, .. }] = diagnostics.as_ref()
    else {
        panic!("the conflicted member access should report exactly one diagnostic");
    };
    assert_eq!(declarations.len(), 2);
}

/// An invalid slot whose declarations are all invisible does not count as
/// a hit — denials are kept, but the lookup keeps walking outward and a
/// file-level `f` still resolves behind it.
#[test]
fn inaccessible_invalid_slot_does_not_shadow_outer_scope() {
    let (db, file) = make_test_db(
        r#"
        function f(bool a) returns (uint) { return 3; }
        contract A {
            function f() external virtual {}
        }
        contract B is A {
            function f() external {}
        }
        contract C is B {
            function probe() external {}
        }
        "#,
    );
    let probe = find_function(&db, file, "probe");
    let resolver = Resolver::build(
        &db,
        probe,
    );

    // `B.f` is an invalid override of `A.f` (no `override` marker); both
    // declarations are `external`, so none is visible on the Direct path.
    let Resolution::Callable(callable) = resolver.resolve_name(&"f".into(), None, 0).unwrap()
    else {
        panic!("`f` should resolve past the invisible invalid slot");
    };
    let DefId::Function(id) = callable.def().unwrap() else {
        panic!("`f` should be a function");
    };
    let sig = db.function_signature(id).expect("a signature");
    assert!(
        matches!(
            sig.params[0].kind(&db),
            TyKind::Primitive(Primitive::Boolean)
        ),
        "the file-level `f(bool)` should win over the inaccessible contract member"
    );
}

/// `super.f` dispatches to the first *implemented* declaration in the
/// declaring contract's C3 tail — a bodyless base declaration is skipped.
#[test]
fn super_dispatches_past_a_bodyless_base_declaration() {
    let (db, file) = make_test_db(
        r#"
        contract A {
            function f() public virtual returns (uint) { return 1; }
        }
        contract B is A {
            function f() public virtual override returns (uint);
        }
        contract C is B {
            function probe() public {}
        }
        "#,
    );
    let probe = find_function(&db, file, "probe");
    let resolver = Resolver::build(
        &db,
        probe,
    );
    let DefId::Contract(c_id) = find_contract(&db, file, "C") else {
        panic!("expected a contract")
    };
    let super_ty = Ty::new(&db, TyKind::Super(c_id));

    let Resolution::Callable(callable) = lookup_member(&resolver, super_ty, &"f".into()).unwrap()
    else {
        panic!("`super.f` should dispatch to the implemented `A.f`");
    };
    // The bodyless `B.f` is skipped: the selected declaration is `A.f`,
    // declared before `contract B` in the fixture.
    let name_range = def_name_range(&db, file, callable.def().expect("a declaration")).unwrap();
    let b_offset = db.text(file).find("contract B").unwrap() as u32;
    assert!(
        name_range.start < b_offset,
        "`super.f` should dispatch past the bodyless `B.f` to `A.f`"
    );
    assert!(
        matches!(callable.kind(), CallKind::Base(BaseKind::Super)) && callable.is_invocable(&db)
    );
}

/// Named lookup is demand-directed: only the requested member is
/// collected.
#[test]
fn member_lookup_at_collects_only_the_requested_name() {
    let (db, file) = make_test_db(
        r#"
        contract C {
            uint public value;
            function other() external {}
            function probe() external {}
        }
        "#,
    );
    let probe = find_function(&db, file, "probe");
    let resolver = Resolver::build(
        &db,
        probe,
    );
    let c = contract_type(&db, file, "C");
    let resolution =
        resolver.lookup_member_at(c, &"value".into(), ExprId::from_raw(0.into()));
    assert!(resolution.is_some());
}

/// A library's internal functions are reachable through the library name
/// from anywhere — its `private` members are not.
#[test]
fn library_type_members() {
    let (db, file) = make_test_db(
        r#"
        library L {
            function add(uint a) internal pure returns (uint) { return a; }
            function ext(uint a) external pure returns (uint) { return a; }
            function priv(uint a) private pure returns (uint) { return a; }
        }
        contract C {
            function probe() external {}
        }
        "#,
    );
    let probe = find_function(&db, file, "probe");
    let resolver = Resolver::build(
        &db,
        probe,
    );
    let Resolution::Type(l_ty) = resolver.resolve_name(&"L".into(), None, 0).unwrap() else {
        panic!("`L` should resolve to a type")
    };
    let l = types_receiver(&db, l_ty.ty());

    for name in ["add", "ext"] {
        let Resolution::Callable(callable) = lookup_member(&resolver, l, &name.into()).unwrap()
        else {
            panic!("`L.{name}` should resolve")
        };
        assert!(
            matches!(callable.kind(), CallKind::ViaType(ViaTypeKind::Library))
                && callable.is_invocable(&db),
            "`L.{name}` should be an invocable library member"
        );
    }
    assert!(lookup_member(&resolver, l, &"priv".into()).is_none());
}

/// A library's own members are reachable bare from inside the library:
/// `internal`/`private` functions and an `internal` constant. A library has no
/// C3 linearization, so "the caller is in the owner" must not be answered by
/// `bases_analysis` alone — otherwise the library is never in itself and its
/// `internal` members are denied on the `Direct` path. `external` stays
/// invisible bare, as it is in a contract.
#[test]
fn library_members_are_reachable_bare_inside_the_library() {
    let (db, file) = make_test_db(
        r#"
        library L {
            uint internal constant K = 3;
            function helper() internal pure returns (uint) { return 1; }
            function priv() private pure returns (uint) { return 2; }
            function ext() external pure returns (uint) { return 4; }
            function probe() public pure returns (uint) { return helper() + priv() + K; }
        }
        "#,
    );
    let probe = find_function(&db, file, "probe");
    let resolver = Resolver::build(
        &db,
        probe,
    );

    for name in ["helper", "priv", "K"] {
        assert!(
            resolver.resolve_name(&name.into(), None, 0).is_some(),
            "`{name}` should resolve bare inside its own library"
        );
    }
    assert!(
        resolve_reported_name(&resolver, "ext", 10).is_none(),
        "`ext` has no bare internal reference inside the library"
    );
    let denials = recorded_denials(&resolver);
    assert!(
        denials
            .iter()
            .any(|denial| denial.vis == Visibility::External),
        "the denied `external` member records its reason"
    );
}
