use crate::hir::semantic_errors::{DiagnosticCode, SemanticDiagnostic};
use crate::hir::tests::helper::{find_contract, make_test_db};
use crate::salsa::HirDb;

fn codes(source: &str, contract: &str) -> Vec<DiagnosticCode> {
    let (db, file) = make_test_db(source);
    let def = find_contract(&db, file, contract);
    db.member_map(def)
        .diagnostics
        .iter()
        .map(SemanticDiagnostic::code)
        .collect()
}

#[test]
fn valid_override_has_no_diagnostics() {
    assert!(
        codes(
            r#"
        contract A { function f() public virtual {} }
        contract B is A { function f() public override {} }
        "#,
            "B",
        )
        .is_empty()
    );
}

#[test]
fn single_base_override_without_marker_is_reported() {
    assert_eq!(
        codes(
            r#"
            contract A { function f() public virtual {} }
            contract B is A { function f() public {} }
            "#,
            "B",
        ),
        vec![DiagnosticCode::MissingOverride]
    );
}

#[test]
fn single_interface_override_may_omit_the_marker() {
    assert!(
        codes(
            r#"
        interface I { function f() external; }
        contract B is I { function f() external {} }
        "#,
            "B",
        )
        .is_empty()
    );
}

#[test]
fn two_interfaces_require_the_marker() {
    assert_eq!(
        codes(
            r#"
            interface I { function f() external; }
            interface J { function f() external; }
            contract B is I, J { function f() external {} }
            "#,
            "B",
        ),
        vec![DiagnosticCode::MissingOverride]
    );
}

#[test]
fn marker_without_an_inherited_declaration_is_reported() {
    assert_eq!(
        codes(
            r#"
            contract A {}
            contract B is A { function f() public override {} }
            "#,
            "B",
        ),
        vec![DiagnosticCode::UnnecessaryOverride]
    );
}

#[test]
fn overriding_a_non_virtual_function_is_reported() {
    assert_eq!(
        codes(
            r#"
            contract A { function f() public {} }
            contract B is A { function f() public override {} }
            "#,
            "B",
        ),
        vec![DiagnosticCode::NotVirtual]
    );
}

#[test]
fn incompatible_return_is_reported_once_per_pair() {
    assert_eq!(
        codes(
            r#"
            contract A { function r() public virtual returns (uint256) { return 1; } }
            contract B is A { function r() public override returns (bool) { return true; } }
            "#,
            "B",
        ),
        vec![DiagnosticCode::IncompatibleOverride]
    );
}

#[test]
fn unrelated_siblings_without_resolution_conflict() {
    assert_eq!(
        codes(
            r#"
            contract A { function f() public virtual {} }
            contract B { function f() public virtual {} }
            contract C is A, B {}
            "#,
            "C",
        ),
        vec![DiagnosticCode::OverrideConflict]
    );
}

#[test]
fn unrelated_siblings_resolved_by_own_override_are_accepted() {
    assert!(
        codes(
            r#"
        contract A { function f() public virtual {} }
        contract B { function f() public virtual {} }
        contract C is A, B { function f() public override(A, B) {} }
        "#,
            "C",
        )
        .is_empty()
    );
}

#[test]
fn intermediate_override_does_not_resolve_a_sibling_path() {
    assert_eq!(
        codes(
            r#"
            contract A { function f() public virtual {} }
            contract B is A { function f() public virtual override {} }
            contract C is A, B {}
            "#,
            "C",
        ),
        vec![DiagnosticCode::OverrideConflict]
    );
}

#[test]
fn single_inheritance_chain_resolves_through_its_intermediate_override() {
    assert!(
        codes(
            r#"
        contract A { function f() public virtual {} }
        contract B is A { function f() public virtual override {} }
        contract C is B {}
        "#,
            "C",
        )
        .is_empty()
    );
}

#[test]
fn shared_base_diamond_is_not_a_conflict() {
    assert!(
        codes(
            r#"
        contract A { function f() public virtual {} }
        contract B is A {}
        contract C is A {}
        contract D is B, C {}
        "#,
            "D",
        )
        .is_empty()
    );
}

#[test]
fn diamond_override_lists_every_owner() {
    assert!(
        codes(
            r#"
        contract A { function f() public virtual {} }
        contract B is A { function f() public virtual override {} }
        contract C is A {}
        contract D is B, C { function f() public override(B, A) {} }
        "#,
            "D",
        )
        .is_empty()
    );
}

#[test]
fn diamond_override_list_must_name_every_owner() {
    assert_eq!(
        codes(
            r#"
            contract A { function f() public virtual {} }
            contract B is A { function f() public virtual override {} }
            contract C is A {}
            contract D is B, C { function f() public override(B) {} }
            "#,
            "D",
        ),
        vec![DiagnosticCode::OverrideListMismatch]
    );
}

#[test]
fn implemented_function_replaced_by_bodyless_is_reported() {
    assert_eq!(
        codes(
            r#"
            contract A { function f() public virtual {} }
            abstract contract B is A { function f() public virtual override; }
            "#,
            "B",
        ),
        vec![DiagnosticCode::UnimplementedOverride]
    );
}

#[test]
fn concrete_contract_with_outstanding_obligation_is_reported() {
    assert_eq!(
        codes(
            r#"
            abstract contract A { function f() public virtual; }
            contract B is A {}
            "#,
            "B",
        ),
        vec![DiagnosticCode::UnimplementedContract]
    );
}

#[test]
fn implemented_intermediate_override_discharges_an_obligation() {
    assert!(
        codes(
            r#"
        abstract contract A { function f() public virtual; }
        abstract contract B is A { function f() public virtual override {} }
        contract C is B {}
        "#,
            "C",
        )
        .is_empty()
    );
}

#[test]
fn unimplemented_sibling_beside_implemented_sibling_is_a_conflict_only() {
    assert_eq!(
        codes(
            r#"
            abstract contract A { function f() public virtual; }
            abstract contract B { function f() public virtual {} }
            contract C is A, B {}
            "#,
            "C",
        ),
        vec![DiagnosticCode::OverrideConflict]
    );
}

#[test]
fn abstract_contract_keeps_outstanding_obligations() {
    assert!(
        codes(
            r#"
        abstract contract A { function f() public virtual; }
        abstract contract B is A {}
        "#,
            "B",
        )
        .is_empty()
    );
}

#[test]
fn public_state_variable_cannot_be_overridden() {
    assert_eq!(
        codes(
            r#"
            contract A { uint256 public x; }
            contract B is A { function x() external view override returns (uint256) { return 1; } }
            "#,
            "B",
        ),
        vec![DiagnosticCode::IncompatibleOverride]
    );
}

#[test]
fn getter_may_override_an_external_function() {
    assert!(
        codes(
            r#"
        contract A { function f() external view virtual returns (uint256) { return 1; } }
        contract B is A { uint256 public override f; }
        "#,
            "B",
        )
        .is_empty()
    );
}

#[test]
fn modifier_parameter_mismatch_is_reported() {
    assert_eq!(
        codes(
            r#"
            contract A { modifier m(uint256 x) virtual { _; } }
            contract B is A { modifier m(bool b) override { _; } }
            "#,
            "B",
        ),
        vec![DiagnosticCode::IncompatibleOverride]
    );
}

#[test]
fn unresolved_base_suppresses_override_diagnostics() {
    // The unresolved base itself is reported; no override diagnostic follows.
    let override_codes: Vec<_> = codes(
        r#"
        contract A { function f() public virtual {} }
        contract B is Missing { function f() public {} }
        "#,
        "B",
    )
    .into_iter()
    .filter(|code| *code != DiagnosticCode::InheritanceFailure)
    .collect();
    assert!(override_codes.is_empty(), "{override_codes:?}");
}

#[test]
fn unavailable_signature_does_not_produce_an_override() {
    assert!(
        codes(
            r#"
        contract A { function q(Missing m) public virtual {} }
        contract B is A { function q(Missing m) public {} }
        "#,
            "B",
        )
        .is_empty()
    );
}

#[test]
fn implemented_modifier_replaced_by_bodyless_is_reported() {
    assert_eq!(
        codes(
            r#"
            abstract contract A { modifier m() virtual { _; } }
            abstract contract B is A { modifier m() override virtual; }
            "#,
            "B",
        ),
        vec![DiagnosticCode::UnimplementedOverride]
    );
}

#[test]
fn unimplemented_modifier_override_is_legal() {
    assert!(
        codes(
            r#"
            abstract contract A { modifier m() virtual; }
            abstract contract B is A { modifier m() override virtual; }
            "#,
            "B",
        )
        .is_empty()
    );
}

#[test]
fn concrete_contract_with_unimplemented_modifier_is_reported() {
    assert_eq!(
        codes(
            r#"
            abstract contract A { modifier m() virtual; }
            contract B is A {}
            "#,
            "B",
        ),
        vec![DiagnosticCode::UnimplementedContract]
    );
}
