use camino::Utf8PathBuf;

use crate::hir::member_map::*;
use crate::hir::builtins::MemberNamespace;
use crate::ir::def_map::DefNamespace;
use crate::hir::semantic_errors::{DiagnosticCode, SemanticDiagnostic};
use crate::hir::tests::helper::{find_contract, find_function, make_test_db};
use crate::salsa::HirDb;

/// Inheritance diagnostics belong to the contract's member map, so a
/// contract reports its unresolved bases even when nothing uses them.
#[test]
fn unresolved_base_is_reported_by_member_map() {
    let (_, diagnostics) = analyze("contract Child is MissingBase {}", "Child");
    assert!(diagnostics.iter().any(|diagnostic| {
        matches!(diagnostic, SemanticDiagnostic::InheritanceFailure { problems, .. } if problems.iter().any(|problem| problem.contains("unresolved base")))
    }));
}

#[test]
fn overloaded_override_conflicts_are_reported_per_slot() {
    let (_, diagnostics) = analyze(
        r#"
        contract A { function f(uint) public virtual {} function f(bool) public virtual {} }
        contract B { function f(uint) public virtual {} function f(bool) public virtual {} }
        contract C is A, B {}
        "#,
        "C",
    );
    let conflicts: Vec<_> = diagnostics
        .iter()
        .filter_map(|diagnostic| match diagnostic {
            SemanticDiagnostic::OverrideConflict { declarations, .. } => Some(declarations.len()),
            _ => None,
        })
        .collect();
    assert_eq!(conflicts, vec![2, 2]);
}

#[test]
fn overloaded_event_conflicts_are_reported_per_slot() {
    let (_, diagnostics) = analyze(
        r#"
        contract A { event E(uint256 x); event E(bool x); }
        contract B { event E(uint256 y); event E(bool y); }
        contract C is A, B {}
        "#,
        "C",
    );
    let conflicts: Vec<_> = diagnostics
        .iter()
        .filter_map(|diagnostic| match diagnostic {
            SemanticDiagnostic::InheritedMemberConflict { declarations, .. } => {
                Some(declarations.len())
            }
            _ => None,
        })
        .collect();
    assert_eq!(conflicts, vec![2, 2]);
}

#[test]
fn every_unresolved_base_is_reported() {
    let (_, diagnostics) = analyze("contract X is M1, M2 {}", "X");
    let problems: Vec<String> = diagnostics
        .iter()
        .filter_map(|diagnostic| match diagnostic {
            SemanticDiagnostic::InheritanceFailure { problems, .. } => Some(problems.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(problems.len(), 2, "{problems:?}");
    assert!(
        problems.iter().any(|problem| problem.contains("`M1`"))
            && problems.iter().any(|problem| problem.contains("`M2`")),
        "{problems:?}"
    );
}

/// Member inference does not repeat the contract's inheritance diagnostics.
#[test]
fn function_inference_inside_a_broken_contract_has_no_base_diagnostics() {
    let (db, file) = make_test_db(
        "contract Child is MissingBase { function f() external pure returns (uint) { return 1; } }",
    );
    let inference = db.inference(find_function(&db, file, "f"));
    let codes: Vec<_> = inference.diagnostics.iter().map(SemanticDiagnostic::code).collect();
    assert!(codes.is_empty(), "{codes:?}");
}

fn analyze(source: &str, contract: &str) -> (MemberMap, Vec<SemanticDiagnostic>) {
    let (db, file) = make_test_db(source);
    let def = find_contract(&db, file, contract);
    let result = db.member_map(def);
    (result.data.as_ref().clone(), result.diagnostics.to_vec())
}

fn value_slots<'a>(members: &'a MemberMap, name: &str) -> &'a [MemberSlot] {
    members.get(name, MemberNamespace::Values)
}

fn type_slots<'a>(members: &'a MemberMap, name: &str) -> &'a [MemberSlot] {
    members.get(name, MemberNamespace::Types)
}

fn has_code(diagnostics: &[SemanticDiagnostic], code: DiagnosticCode) -> bool {
    diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code() == code)
}

#[test]
fn inherited_overload_survives_alongside_own_overload() {
    let (db, file) = make_test_db(
        r#"
        contract A {
            function f(uint8) public virtual {}
            function f(bool) public virtual {}
        }
        contract B is A { function f(uint8) public override {} }
        "#,
    );
    let derived = find_contract(&db, file, "B");
    let members = db.member_map(derived);
    assert!(members.diagnostics.is_empty());
    let slots = value_slots(&members.data, "f");
    assert_eq!(slots.len(), 2);
    assert!(slots.iter().any(|slot| {
        slot.effective()
            .is_some_and(|def| db.owner(def) == Some(derived))
    }));
    assert!(slots.iter().all(|slot| slot.effective().is_some()));
}

#[test]
fn override_retains_lineage_and_selects_the_derived_declaration() {
    let (members, diagnostics) = analyze(
        r#"
        contract A { function f(uint256) public virtual {} }
        contract B is A { function f(uint256) public override {} }
        "#,
        "B",
    );
    assert!(diagnostics.is_empty());
    let [slot] = value_slots(&members, "f") else {
        panic!("expected one function slot");
    };
    assert!(slot.effective().is_some());
    assert_eq!(slot.candidates.len(), 2);
    let effective = slot
        .effective()
        .expect("valid slot has an effective declaration");
    let derived = members
        .declarations
        .get(&effective)
        .expect("effective declaration is in its provenance");
    assert_eq!(derived.def, effective);
}

#[test]
fn invalid_override_keeps_its_declaration_as_the_slot_target() {
    let (members, diagnostics) = analyze(
        r#"
        contract A { function f() public {} }
        contract B is A { function f() public override {} }
        "#,
        "B",
    );
    assert!(has_code(&diagnostics, DiagnosticCode::NotVirtual));
    let [slot] = value_slots(&members, "f") else {
        panic!("expected one function slot");
    };
    assert!(slot.effective().is_some());
}

#[test]
fn unresolved_sibling_inheritance_keeps_all_candidates_without_a_winner() {
    let (members, diagnostics) = analyze(
        r#"
        contract A { function f() public virtual {} }
        contract B { function f() public virtual {} }
        contract C is A, B {}
        "#,
        "C",
    );
    assert!(has_code(&diagnostics, DiagnosticCode::OverrideConflict));
    let [slot] = value_slots(&members, "f") else {
        panic!("expected one conflicting slot");
    };
    assert_eq!(slot.effective(), None);
    assert_eq!(slot.candidates.len(), 2);
}

#[test]
fn distinct_conflicts_on_one_contract_all_report() {
    let (_, diagnostics) = analyze(
        r#"
        contract A { function f() public virtual {} function g() public virtual {} }
        contract B { function f() public virtual {} function g() public virtual {} }
        contract C is A, B {}
        "#,
        "C",
    );
    let conflicts: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code() == DiagnosticCode::OverrideConflict)
        .collect();
    // Both slots anchor on the contract name and share a `(code, range)` key;
    // this code intentionally permits one diagnostic for each signature.
    assert_eq!(conflicts.len(), 2);
    assert!(conflicts.iter().all(|d| d.range() == conflicts[0].range()));
    for member in ["f", "g"] {
        assert!(conflicts.iter().any(|diagnostic| {
            matches!(*diagnostic, SemanticDiagnostic::OverrideConflict { name, .. } if name.as_str() == member)
        }));
    }
}

#[test]
fn shared_base_diamond_deduplicates_the_declaration() {
    let (members, diagnostics) = analyze(
        r#"
        contract A { function f() public virtual {} }
        contract B is A {}
        contract C is A {}
        contract D is B, C {}
        "#,
        "D",
    );
    assert!(diagnostics.is_empty());
    let [slot] = value_slots(&members, "f") else {
        panic!("expected one function slot");
    };
    assert_eq!(slot.candidates.len(), 1);
    assert!(slot.effective().is_some());
}

#[test]
fn getter_and_external_function_share_one_callable_slot() {
    let (members, diagnostics) = analyze(
        r#"
        contract A { function total() external view virtual returns (uint256) { return 1; } }
        contract B is A { uint256 public override total; }
        "#,
        "B",
    );
    assert!(diagnostics.is_empty());
    let [slot] = value_slots(&members, "total") else {
        panic!("expected one getter/function slot");
    };
    assert!(slot.effective().is_some());
    assert_eq!(slot.candidates.len(), 2);
    let declarations: Vec<_> = slot
        .candidates
        .iter()
        .map(|def| &members.declarations[def])
        .collect();
    assert!(
        declarations
            .iter()
            .any(|decl| decl.namespace == DefNamespace::Functions)
    );
    assert!(
        declarations
            .iter()
            .any(|decl| decl.namespace == DefNamespace::Variables)
    );
    assert_eq!(
        slot.effective(),
        declarations
            .iter()
            .find(|decl| decl.namespace == DefNamespace::Variables)
            .map(|decl| decl.def)
    );
}

#[test]
fn outstanding_abstract_declaration_is_retained_as_an_obligation() {
    let (members, diagnostics) = analyze(
        r#"
        abstract contract A { function f() public virtual; }
        abstract contract B is A {}
        "#,
        "B",
    );
    assert!(diagnostics.is_empty());
    let [slot] = value_slots(&members, "f") else {
        panic!("expected one abstract slot");
    };
    assert!(slot.effective().is_some());
    assert_eq!(members.obligations.len(), 1);
    assert_eq!(members.obligations[0], slot.effective().unwrap());
}

#[test]
fn unavailable_signature_keeps_its_declaration_effective() {
    let (members, _) = analyze(
        r#"
        contract A { function f(Missing value) public virtual {} }
        contract B is A {}
        "#,
        "B",
    );
    let [slot] = value_slots(&members, "f") else {
        panic!("expected one unavailable declaration");
    };
    assert!(slot.effective().is_some());
    assert_eq!(slot.candidates.len(), 1);
}

#[test]
fn unresolved_inheritance_keeps_own_declarations_effective() {
    let (members, _) = analyze(
        r#"
        contract A { function f() public {} }
        contract B is Missing { function g() public {} }
        "#,
        "B",
    );
    assert!(!members.complete);
    let [slot] = value_slots(&members, "g") else {
        panic!("expected the own declaration to be retained");
    };
    assert!(slot.effective().is_some());
}

#[test]
fn type_and_value_names_are_kept_in_separate_member_namespaces() {
    let (members, diagnostics) = analyze(
        r#"
        contract A {
            struct Item { uint256 value; }
            uint256 private state;
        }
        "#,
        "A",
    );
    assert!(diagnostics.is_empty());
    assert_eq!(type_slots(&members, "Item").len(), 1);
    assert_eq!(value_slots(&members, "state").len(), 1);
}

#[test]
fn event_and_error_overloads_remain_distinct() {
    let (members, diagnostics) = analyze(
        r#"
        contract A {
            event E(uint256 indexed value);
            event E(bool value);
            error Bad(uint256 value);
            error Bad(bool value);
        }
        "#,
        "A",
    );
    assert!(diagnostics.is_empty());
    assert_eq!(value_slots(&members, "E").len(), 2);
    assert_eq!(value_slots(&members, "Bad").len(), 2);
}

#[test]
fn inherited_non_overloadable_declarations_remain_an_explicit_conflict() {
    let (members, diagnostics) = analyze(
        r#"
        contract A { struct Item { uint256 a; } }
        contract B { struct Item { uint256 b; } }
        contract C is A, B {}
        "#,
        "C",
    );
    assert!(has_code(
        &diagnostics,
        DiagnosticCode::InheritedMemberConflict
    ));
    let [slot] = type_slots(&members, "Item") else {
        panic!("expected one non-overloadable type slot");
    };
    assert_eq!(slot.effective(), None);
    assert_eq!(slot.candidates.len(), 2);
}

#[test]
fn inherited_non_overloadable_variables_are_kept_as_conflicts() {
    let (members, diagnostics) = analyze(
        r#"
        contract A { uint256 internal state; }
        contract B { uint256 internal state; }
        contract C is A, B {}
        "#,
        "C",
    );
    assert!(has_code(
        &diagnostics,
        DiagnosticCode::InheritedMemberConflict
    ));
    let [slot] = value_slots(&members, "state") else {
        panic!("expected one variable conflict slot");
    };
    assert_eq!(slot.effective(), None);
    assert_eq!(slot.candidates.len(), 2);
}

#[test]
fn private_base_variables_do_not_collide_with_derived() {
    let (members, diagnostics) = analyze(
        r#"
        contract A { uint256 private state; }
        contract B { uint256 private state; }
        contract C is A, B {}
        "#,
        "C",
    );
    assert!(!has_code(
        &diagnostics,
        DiagnosticCode::InheritedMemberConflict
    ));
    let [slot] = value_slots(&members, "state") else {
        panic!("expected one variable slot");
    };
    assert_eq!(slot.effective(), None);
    assert_eq!(slot.candidates.len(), 2);

    let (members, diagnostics) = analyze(
        r#"
        contract A { uint256 private state; }
        contract B is A { uint256 private state; }
        "#,
        "B",
    );
    assert!(diagnostics.is_empty());
    let [slot] = value_slots(&members, "state") else {
        panic!("expected one variable slot");
    };
    assert_eq!(slot.candidates.len(), 2);
    assert!(slot.effective().is_some());
}

#[test]
fn inherited_event_and_error_signature_collisions_are_kept_as_conflicts() {
    let (members, diagnostics) = analyze(
        r#"
        contract A { event E(uint256 x); error Bad(uint256 x); }
        contract B { event E(uint256 y); error Bad(uint256 y); }
        contract C is A, B {}
        "#,
        "C",
    );
    assert_eq!(
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code() == DiagnosticCode::InheritedMemberConflict)
            .count(),
        2
    );
    for name in ["E", "Bad"] {
        let [slot] = value_slots(&members, name) else {
            panic!("expected one conflicting overload slot for {name}");
        };
        assert_eq!(slot.effective(), None);
        assert_eq!(slot.candidates.len(), 2);
    }
}

#[test]
fn body_only_edit_preserves_member_map_data() {
    let source = "contract A { function f() public virtual { uint256 x = 1; } } contract B is A {}";
    let (mut db, file) = make_test_db(source);
    let contract = find_contract(&db, file, "B");
    let before = db.member_map(contract);
    let changed = source.replace("x = 1", "x = 2");
    db.open_file(Utf8PathBuf::from("/tmp/next_tests/src/Test.sol"), changed);
    let contract = find_contract(&db, file, "B");
    let after = db.member_map(contract);
    let unchanged = before.data.as_ref() == after.data.as_ref();
    assert!(unchanged);
}

#[test]
fn duplicate_owned_function_slot_is_conflicting_and_diagnosed() {
    let (members, diagnostics) = analyze(
        r#"
        contract A {
            function f(uint256) public {}
            function f(uint256) public {}
        }
        "#,
        "A",
    );
    assert!(has_code(&diagnostics, DiagnosticCode::DuplicateMemberSlot));
    let [slot] = value_slots(&members, "f") else {
        panic!("expected one duplicate slot");
    };
    assert_eq!(slot.effective(), None);
    assert_eq!(slot.candidates.len(), 2);
}

#[test]
fn payable_members_are_derived_across_the_linearization() {
    let (db, file) = make_test_db(
        r#"
        contract ReceiveBase { receive() external payable {} }
        contract Receiver is ReceiveBase {}
        contract FallbackBase { fallback() external payable {} }
        contract PayableFallback is FallbackBase {}
        contract NonpayableFallback { fallback() external {} }
        "#,
    );
    let is_payable = |name| db.member_map(find_contract(&db, file, name)).data.is_payable(&db);
    assert!(is_payable("Receiver"));
    assert!(is_payable("PayableFallback"));
    assert!(!is_payable("NonpayableFallback"));
}
