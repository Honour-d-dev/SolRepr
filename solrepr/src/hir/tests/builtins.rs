use crate::hir::builtins::*;
use crate::hir::types::{Callee, Location, Primitive, Ty, TyKind, Type};
use crate::salsa::HirDb;
use crate::hir::types::RefKind;
use crate::hir::resolution::Resolution;
use crate::hir::resolver::Resolver;
use crate::hir::tests::helper::*;
use crate::hir::types::{CallKind, ViaTypeKind};
use crate::ir::def_map::DefId;
use crate::salsa::{File, Db};
use num_bigint::BigInt;
use std::borrow::Cow;

fn resolver_for(db: &Db, file: File) -> Resolver<'_> {
    let f = find_function(db, file, "f");
    Resolver::build(db, f)
}

fn names(members: &[Builtin]) -> Vec<&str> {
    members.iter().map(|m| m.name()).collect()
}

/// The removed `BuiltinDb::member` as a test helper — production callers
/// enumerate `members` and match by name.
fn member(builtins: BuiltinDb<'_>, ty: Ty, name: &str, ctx: MemberNamespace) -> Option<Builtin> {
    builtins
        .members(ty, ctx)
        .iter()
        .find(|m| m.name() == name)
        .copied()
}

#[test]
fn member_namespace_splits_bytes_types_and_values() {
    let (db, file) = make_test_db("contract C { function f() external {} }");
    let resolver = resolver_for(&db, file);
    let bytes = Ty::primitive(&db, Primitive::Bytes);

    assert_eq!(
        names(&resolver.builtins().members(bytes, MemberNamespace::Types)),
        ["concat"]
    );
    let value = resolver.builtins().members(bytes, MemberNamespace::Values);
    assert!(names(&value).contains(&"length"));
    assert!(!names(&value).contains(&"concat"));
}

#[test]
fn member_namespace_udvt_wrap_unwrap() {
    let (db, file) = make_test_db("type Price is uint256; contract C { function f() external {} }");
    let resolver = resolver_for(&db, file);
    let udvt = match resolver.resolve_name(&"Price".into(), None, 0) {
        Some(Resolution::Type(ty)) => ty.ty(),
        _ => panic!("Price should resolve to a type"),
    };

    let ns = resolver.builtins().members(udvt, MemberNamespace::Types);
    let ns_names = names(&ns);
    assert!(ns_names.contains(&"wrap"), "got {ns_names:?}");
    assert!(ns_names.contains(&"unwrap"), "got {ns_names:?}");
    assert!(
        resolver
            .builtins()
            .members(udvt, MemberNamespace::Values)
            .is_empty()
    );
}

#[test]
fn global_object_members_are_types_only() {
    let (db, file) = make_test_db("contract C { function f() external {} }");
    let resolver = resolver_for(&db, file);
    let msg = Ty::new(&db, TyKind::Builtin(BuiltinTy::Msg));

    assert!(
        member(resolver.builtins(), msg, "sender", MemberNamespace::Types).is_some()
    );
    assert!(
        member(resolver.builtins(), msg, "sender", MemberNamespace::Values).is_none()
    );
}

#[test]
fn resolve_global_finds_builtin_functions() {
    let (db, file) = make_test_db("contract C { function f() external {} }");
    let resolver = resolver_for(&db, file);

    assert_eq!(
        resolver
            .builtins()
            .resolve_global(&"msg".into())
            .map(Builtin::name),
        Some("msg")
    );
    assert_eq!(
        resolver
            .builtins()
            .resolve_global(&"keccak256".into())
            .map(Builtin::name),
        Some("keccak256")
    );
    assert!(
        resolver
            .builtins()
            .resolve_global(&"nope".into())
            .is_none()
    );
}

#[test]
fn block_and_globals_include_cancun_builtins() {
    let (db, file) = make_test_db("contract C { function f() external {} }");
    let resolver = resolver_for(&db, file);
    let block = Ty::new(&db, TyKind::Builtin(BuiltinTy::Block));

    assert!(
        member(resolver.builtins(), block, "blobbasefee", MemberNamespace::Types).is_some()
    );
    assert!(
        resolver
            .builtins()
            .resolve_global(&"blobhash".into())
            .is_some()
    );
}

#[test]
fn db_dependent_docs_name_the_actual_types() {
    let (db, file) =
        make_test_db("type Price is uint256; contract C { uint256[] s; function f() external {} }");
    let resolver = resolver_for(&db, file);

    let udvt = match resolver.resolve_name(&"Price".into(), None, 0) {
        Some(Resolution::Type(ty)) => ty.ty(),
        _ => panic!("Price should resolve to a type"),
    };
    let unwrap = member(resolver.builtins(), udvt, "unwrap", MemberNamespace::Types)
        .expect("unwrap");
    assert!(
        unwrap.doc(&db).contains("uint256"),
        "got {:?}",
        unwrap.doc(&db)
    );

    let array = Type::new(
        &db,
        TyKind::Array {
            elem: Type::primitive(&db, Primitive::Uint(256)),
            size: None,
        },
    )
    .expect("array is nameable")
    .at_loc(&db, Location::Storage(RefKind::Reference))
    .ty();
    let push = member(resolver.builtins(), array, "push", MemberNamespace::Values).expect("push");
    assert!(
        push.doc(&db).contains("uint256"),
        "got {:?}",
        push.doc(&db)
    );

    let meta = Ty::new(
        &db,
        TyKind::Meta(Type::primitive(&db, Primitive::Uint(256))),
    );
    let max = member(resolver.builtins(), meta, "max", MemberNamespace::Types).expect("max");
    assert!(
        max.doc(&db).contains("uint256"),
        "got {:?}",
        max.doc(&db)
    );
}

#[test]
fn declaration_docs_name_the_declaration() {
    let (db, file) = make_test_db(
        r#"
        contract C {
            error Bad();
            event Done();
            function ping() public {}
            function f() public {}
        }
        "#,
    );
    let resolver = resolver_for(&db, file);
    let def = |name: &str| match resolver.resolve_name(&name.into(), None, 0) {
        Some(Resolution::Callable(callable)) => callable.def().expect("def"),
        _ => panic!("{name} should resolve to a callable"),
    };
    let selector_doc = |ty: Ty| {
        member(resolver.builtins(), ty, "selector", MemberNamespace::Values)
            .expect("selector")
            .doc(&db)
            .to_string()
    };

    let DefId::Error(id) = def("Bad") else {
        panic!("Bad is an error");
    };
    assert!(selector_doc(Ty::new(&db, TyKind::Error(id))).contains("Bad"));

    let DefId::Event(id) = def("Done") else {
        panic!("Done is an event");
    };
    assert!(selector_doc(Ty::new(&db, TyKind::Event(id))).contains("Done"));

    let DefId::Function(id) = def("ping") else {
        panic!("ping is a function");
    };
    let fty = Ty::new(
        &db,
        TyKind::Function {
            callee: Callee::Fn(id),
            kind: CallKind::ViaType(ViaTypeKind::NonLibrary),
        },
    );
    assert!(selector_doc(fty).contains("ping"));
}

/// Static member tables are `Cow::Borrowed`; the receiver-parameterized sets
/// (a storage `T[]`'s `push`/`pop`, `type(T).*`) are `Cow::Owned`.
#[test]
fn static_tables_are_borrowed_parameterized_sets_owned() {
    let (db, file) = make_test_db("contract C { function f() external {} }");
    let resolver = resolver_for(&db, file);

    // Fully-static tables borrow their `'static` table — no allocation.
    let address = Ty::primitive(&db, Primitive::Address);
    assert!(matches!(
        resolver
            .builtins()
            .members(address, MemberNamespace::Values),
        Cow::Borrowed(_)
    ));
    // A fixed-size array is `.length`-only → static.
    let fixed = Ty::new(
        &db,
        TyKind::Array {
            elem: Type::primitive(&db, Primitive::Uint(256)),
            size: Some(BigInt::from(3u8)),
        },
    );
    assert!(matches!(
        resolver.builtins().members(fixed, MemberNamespace::Values),
        Cow::Borrowed(_)
    ));
    // `address payable` is one combined static table now that member sets
    // are `Copy` ids — still borrowed.
    let payable = Ty::primitive(&db, Primitive::AddressPayable);
    assert!(matches!(
        resolver
            .builtins()
            .members(payable, MemberNamespace::Values),
        Cow::Borrowed(_)
    ));
    // A storage `T[]` specializes `push`/`pop` with the element `Type` →
    // owned.
    let array = Type::new(
        &db,
        TyKind::Array {
            elem: Type::primitive(&db, Primitive::Uint(256)),
            size: None,
        },
    )
    .expect("array is nameable")
    .at_loc(&db, Location::Storage(RefKind::Reference))
    .ty();
    assert!(matches!(
        resolver.builtins().members(array, MemberNamespace::Values),
        Cow::Owned(_)
    ));
}

#[test]
fn owner_query_resolves_the_enclosing_container() {
    // The conversion rules ask `db.owner` instead of walking the def-map.
    let (db, file) = make_test_db(
        r#"
        contract C {
            function f() external {}
        }
        "#,
    );
    let f = find_function(&db, file, "f");
    let c = find_contract(&db, file, "C");
    assert_eq!(db.owner(f), Some(c));
    // A top-level item's owner is the file.
    assert_eq!(db.owner(c), Some(DefId::File(file)));
}
