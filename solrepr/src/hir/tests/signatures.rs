use crate::hir::signatures::*;
use crate::ir::def_map::DefId;
use crate::salsa::hir_db::HirDb;
use crate::hir::exprs::Name;
use crate::hir::tests::helper::{find_contract, make_test_db};
use crate::salsa::root_db::RootDb;
use crate::salsa::{File, Db};

/// The `index`-th declaration named `name` directly inside `contract`.
fn member(db: &Db, file: File, contract: &str, name: &str, index: usize) -> DefId {
    let contract = find_contract(db, file, contract);
    let defmap = db.file_def_map(file);
    let scope = defmap
        .defs
        .get(&contract)
        .and_then(|data| data.child_scope)
        .expect("contract scope");
    let name: Name = name.into();
    defmap.scopes[scope].by_name[&name].defs[index]
}

fn slot(db: &Db, file: File, contract: &str, name: &str) -> Option<OverrideSignature> {
    OverrideSignature::of(db, member(db, file, contract, name, 0))
}

fn same_slot(db: &Db, file: File, derived: (&str, &str), base: (&str, &str)) -> bool {
    match (
        slot(db, file, derived.0, derived.1),
        slot(db, file, base.0, base.1),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

fn compatibility(
    db: &Db,
    file: File,
    derived: (&str, &str),
    base: (&str, &str),
) -> Option<Vec<OverrideMismatch>> {
    OverrideSignature::compatibility(
        db,
        member(db, file, derived.0, derived.1, 0),
        member(db, file, base.0, base.1, 0),
    )
    .map(Into::into)
}

fn fixture(source: &str) -> (Db, File) {
    make_test_db(source)
}

#[test]
fn identity_ignores_returns_and_parameter_names() {
    let (db, file) = fixture(
        r#"
        contract A { function f(uint256 x) public virtual returns (uint256) { return x; } }
        contract B is A { function f(uint256 y) public override returns (bool) { return true; } }
        "#,
    );
    assert!(same_slot(&db, file, ("B", "f"), ("A", "f")));
}

#[test]
fn identity_keeps_overloads_apart() {
    let (db, file) = fixture(
        r#"
        contract C {
            function h(uint256 x) public {}
            function h(bool b) public {}
        }
        "#,
    );
    let first = OverrideSignature::of(&db, member(&db, file, "C", "h", 0));
    let second = OverrideSignature::of(&db, member(&db, file, "C", "h", 1));
    assert!(matches!((first, second), (Some(a), Some(b)) if a != b));
}

#[test]
fn identity_treats_calldata_and_memory_as_one_parameter_type() {
    let (db, file) = fixture(
        r#"
        contract D { function k(bytes calldata b) external virtual {} }
        contract E is D { function k(bytes memory b) public override {} }
        "#,
    );
    assert!(same_slot(&db, file, ("E", "k"), ("D", "k")));
}

#[test]
fn identity_keeps_storage_distinct_from_memory() {
    let (db, file) = fixture(
        r#"
        contract F { function s(uint256[] storage a) internal virtual {} }
        contract G is F { function s(uint256[] memory a) internal override {} }
        "#,
    );
    assert!(slot(&db, file, "F", "s").is_some());
    assert!(slot(&db, file, "G", "s").is_some());
    assert!(!same_slot(&db, file, ("G", "s"), ("F", "s")));
}

#[test]
fn identity_lets_a_getter_occupy_an_external_function_slot() {
    let (db, file) = fixture(
        r#"
        contract H { function total() external view virtual returns (uint256) { return 1; } }
        contract I is H { uint256 public override total; }
        "#,
    );
    assert!(same_slot(&db, file, ("I", "total"), ("H", "total")));
}

#[test]
fn identity_separates_fallback_from_receive() {
    let (db, file) = fixture(
        r#"
        contract J {
            fallback() external {}
            receive() external payable {}
        }
        "#,
    );
    let first = OverrideSignature::of(&db, member(&db, file, "J", "fallback", 0));
    let second = OverrideSignature::of(&db, member(&db, file, "J", "receive", 0));
    assert!(matches!((first, second), (Some(a), Some(b)) if a != b));
}

#[test]
fn identity_names_modifiers_without_their_parameters() {
    let (db, file) = fixture(
        r#"
        contract M { modifier m(uint256 x) virtual { _; } }
        contract N is M { modifier m(bool b) override { _; } }
        "#,
    );
    assert!(same_slot(&db, file, ("N", "m"), ("M", "m")));
}

#[test]
fn constructors_have_no_override_slot() {
    let (db, file) = fixture(
        r#"
        contract O { constructor(uint256 a) {} }
        "#,
    );
    let DefId::Contract(id) = find_contract(&db, file, "O") else {
        panic!("expected a contract");
    };
    let constructor = db
        .contract_data(id)
        .constructor
        .expect("contract should declare a constructor");
    assert!(OverrideSignature::of(&db, DefId::Function(constructor)).is_none());
}

#[test]
fn non_public_state_variables_have_no_override_slot() {
    let (db, file) = fixture(
        r#"
        contract R { uint256 internal hidden; }
        "#,
    );
    assert!(slot(&db, file, "R", "hidden").is_none());
}

#[test]
fn unavailable_parameter_types_have_no_slot_and_no_comparison() {
    let (db, file) = fixture(
        r#"
        contract P { function q(Missing m) public virtual {} }
        contract Q is P { function q(Missing m) public override {} }
        "#,
    );
    assert!(slot(&db, file, "P", "q").is_none());
    assert!(compatibility(&db, file, ("Q", "q"), ("P", "q")).is_none());
}

#[test]
fn compatible_override_has_no_mismatch() {
    let (db, file) = fixture(
        r#"
        contract A { function f(uint256 x) public virtual returns (uint256) { return x; } }
        contract B is A { function f(uint256 y) public override returns (uint256) { return y; } }
        "#,
    );
    assert_eq!(
        compatibility(&db, file, ("B", "f"), ("A", "f")),
        Some(vec![])
    );
}

#[test]
fn different_slots_are_not_compared() {
    let (db, file) = fixture(
        r#"
        contract C {
            function h(uint256 x) public {}
            function h(bool b) public {}
        }
        "#,
    );
    let result = OverrideSignature::compatibility(
        &db,
        member(&db, file, "C", "h", 0),
        member(&db, file, "C", "h", 1),
    );
    assert!(result.is_none());
}

#[test]
fn return_type_mismatch_is_reported() {
    let (db, file) = fixture(
        r#"
        contract S1 { function r(uint256 x) public virtual returns (uint256) { return x; } }
        contract S2 is S1 { function r(uint256 x) public override returns (bool) { return true; } }
        "#,
    );
    let mismatches = compatibility(&db, file, ("S2", "r"), ("S1", "r")).expect("comparable");
    assert!(mismatches.contains(&OverrideMismatch::ReturnTypes));
}

#[test]
fn visibility_may_only_widen_from_external_to_public() {
    let (db, file) = fixture(
        r#"
        contract V1 { function v() external virtual {} }
        contract V2 is V1 { function v() public override {} }
        contract W1 { function w() public virtual {} }
        contract W2 is W1 { function w() external override {} }
        contract T1 { function t() public virtual {} }
        contract T2 is T1 { function t() internal override {} }
        "#,
    );
    assert_eq!(
        compatibility(&db, file, ("V2", "v"), ("V1", "v")),
        Some(vec![])
    );
    assert!(
        compatibility(&db, file, ("W2", "w"), ("W1", "w"))
            .expect("comparable")
            .contains(&OverrideMismatch::Visibility)
    );
    assert!(
        compatibility(&db, file, ("T2", "t"), ("T1", "t"))
            .expect("comparable")
            .contains(&OverrideMismatch::Visibility)
    );
}

#[test]
fn mutability_may_only_become_stricter_and_never_leave_payable() {
    let (db, file) = fixture(
        r#"
        contract X1 { function x() public virtual {} }
        contract X2 is X1 { function x() public view override {} }
        contract Y1 { function y() public view virtual {} }
        contract Y2 is Y1 { function y() public override {} }
        contract Z1 { function z() public payable virtual {} }
        contract Z2 is Z1 { function z() public override {} }
        "#,
    );
    assert_eq!(
        compatibility(&db, file, ("X2", "x"), ("X1", "x")),
        Some(vec![])
    );
    assert!(
        compatibility(&db, file, ("Y2", "y"), ("Y1", "y"))
            .expect("comparable")
            .contains(&OverrideMismatch::Mutability)
    );
    assert!(
        compatibility(&db, file, ("Z2", "z"), ("Z1", "z"))
            .expect("comparable")
            .contains(&OverrideMismatch::Mutability)
    );
}

#[test]
fn calldata_to_memory_is_accepted_only_for_external_bases() {
    let (db, file) = fixture(
        r#"
        contract AA1 { function a(bytes calldata b) external virtual {} }
        contract AA2 is AA1 { function a(bytes memory b) public override {} }
        contract AB1 { function b(bytes calldata x) internal virtual {} }
        contract AB2 is AB1 { function b(bytes memory x) internal override {} }
        "#,
    );
    assert_eq!(
        compatibility(&db, file, ("AA2", "a"), ("AA1", "a")),
        Some(vec![])
    );
    assert!(
        compatibility(&db, file, ("AB2", "b"), ("AB1", "b"))
            .expect("comparable")
            .contains(&OverrideMismatch::ParameterLocations)
    );
}

#[test]
fn return_locations_must_match_for_non_external_bases() {
    let (db, file) = fixture(
        r#"
        contract AJ1 { function aj() internal virtual returns (bytes memory r) { return r; } }
        contract AJ2 is AJ1 { function aj() internal override returns (bytes calldata r) { return r; } }
        contract AK1 { function ak() external virtual returns (bytes memory r) { return r; } }
        contract AK2 is AK1 { function ak() public override returns (bytes calldata r) { return r; } }
        "#,
    );
    assert!(
        compatibility(&db, file, ("AJ2", "aj"), ("AJ1", "aj"))
            .expect("comparable")
            .contains(&OverrideMismatch::ReturnLocations)
    );
    assert!(
        !compatibility(&db, file, ("AK2", "ak"), ("AK1", "ak"))
            .expect("comparable")
            .contains(&OverrideMismatch::ReturnLocations)
    );
}

#[test]
fn modifier_parameters_must_match_after_the_name_slot() {
    let (db, file) = fixture(
        r#"
        contract AC1 { modifier am(uint256 x) virtual { _; } }
        contract AC2 is AC1 { modifier am(uint256 y) override { _; } }
        contract AD1 { modifier ad(uint256 x) virtual { _; } }
        contract AD2 is AD1 { modifier ad(bool b) override { _; } }
        "#,
    );
    assert_eq!(
        compatibility(&db, file, ("AC2", "am"), ("AC1", "am")),
        Some(vec![])
    );
    assert!(
        compatibility(&db, file, ("AD2", "ad"), ("AD1", "ad"))
            .expect("comparable")
            .contains(&OverrideMismatch::ModifierParameters)
    );
}

#[test]
fn getter_may_override_an_external_function() {
    let (db, file) = fixture(
        r#"
        contract AE1 { function ae() external view virtual returns (uint256) { return 1; } }
        contract AE2 is AE1 { uint256 public override ae; }
        "#,
    );
    assert_eq!(
        compatibility(&db, file, ("AE2", "ae"), ("AE1", "ae")),
        Some(vec![])
    );
}

#[test]
fn getter_cannot_override_a_non_external_function() {
    let (db, file) = fixture(
        r#"
        contract AF1 { function af() internal view virtual returns (uint256) { return 1; } }
        contract AF2 is AF1 { uint256 public override af; }
        "#,
    );
    assert!(
        compatibility(&db, file, ("AF2", "af"), ("AF1", "af"))
            .expect("comparable")
            .contains(&OverrideMismatch::GetterRequiresExternalBase)
    );
}

#[test]
fn public_state_variable_cannot_be_overridden() {
    let (db, file) = fixture(
        r#"
        contract AG1 { uint256 public ag; }
        contract AG2 is AG1 { function ag() external view override returns (uint256) { return 1; } }
        "#,
    );
    assert!(
        compatibility(&db, file, ("AG2", "ag"), ("AG1", "ag"))
            .expect("comparable")
            .contains(&OverrideMismatch::GetterCannotBeOverridden)
    );
}

#[test]
fn constant_getter_is_pure_and_may_override_a_pure_function() {
    let (db, file) = fixture(
        r#"
        contract AH1 { function ah() external pure virtual returns (uint256) { return 1; } }
        contract AH2 is AH1 { uint256 public constant override ah = 1; }
        contract AI1 { function ai() external pure virtual returns (uint256) { return 1; } }
        contract AI2 is AI1 { uint256 public override ai; }
        "#,
    );
    assert_eq!(
        compatibility(&db, file, ("AH2", "ah"), ("AH1", "ah")),
        Some(vec![])
    );
    assert!(
        compatibility(&db, file, ("AI2", "ai"), ("AI1", "ai"))
            .expect("comparable")
            .contains(&OverrideMismatch::Mutability)
    );
}
