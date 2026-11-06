use crate::capabilities::diagnostics::*;
use crate::ast::NodeRange;
use crate::hir::diagnostics::{
    Diagnostic, RelatedDiagnostic, Severity, normalize_diagnostics,
};
use crate::hir::diagnostics::DiagnosticCode;
use crate::hir::tests::helper::make_test_db;
use tree_sitter::Parser;

#[test]
fn collects_error_nodes() {
    let source = "contract C { function f() public {";
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_solidity::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let mut diagnostics = Vec::new();

    collect_parse_errors(tree.root_node(), source, &mut diagnostics);

    assert!(!diagnostics.is_empty());
    assert!(diagnostics.len() <= MAX_PARSE_DIAGNOSTICS);
    assert!(diagnostics.iter().all(|diagnostic| {
        diagnostic.code == DiagnosticCode::SyntaxError && diagnostic.severity == Severity::Error
    }));
}

#[test]
fn normalizes_diagnostics_in_source_order_and_removes_duplicates() {
    let diagnostic = |range: NodeRange, code: DiagnosticCode, message: &str| Diagnostic {
        code,
        severity: Severity::Error,
        range,
        message: message.to_string(),
        related: Vec::new(),
    };
    let duplicate = diagnostic(
        NodeRange { start: 4, end: 5 },
        DiagnosticCode::UnresolvedName,
        "unresolved name `x`",
    );
    let mut diagnostics = vec![
        duplicate.clone(),
        diagnostic(
            NodeRange { start: 0, end: 1 },
            DiagnosticCode::SyntaxError,
            "syntax error",
        ),
        duplicate,
    ];

    normalize_diagnostics(&mut diagnostics);

    assert_eq!(diagnostics.len(), 2);
    assert_eq!(diagnostics[0].range, NodeRange { start: 0, end: 1 });
    assert_eq!(diagnostics[1].range, NodeRange { start: 4, end: 5 });
}

#[test]
fn identical_diagnostics_merge_and_deduplicate_related_links() {
    let (_db, file) = make_test_db("contract C {}");
    let range = NodeRange { start: 4, end: 5 };
    let link = |start: u32, message: &str| RelatedDiagnostic {
        file,
        range: NodeRange {
            start,
            end: start + 1,
        },
        message: message.to_string(),
    };
    let mut diagnostics = vec![
        Diagnostic {
            code: DiagnosticCode::UnresolvedName,
            severity: Severity::Error,
            range,
            message: "unresolved name `f`".to_string(),
            related: vec![link(0, "first declaration")],
        },
        Diagnostic {
            code: DiagnosticCode::UnresolvedName,
            severity: Severity::Error,
            range,
            message: "unresolved name `f`".to_string(),
            related: vec![link(0, "first declaration"), link(2, "second declaration")],
        },
        Diagnostic {
            code: DiagnosticCode::UnresolvedName,
            severity: Severity::Error,
            range,
            message: "unresolved name `g`".to_string(),
            related: Vec::new(),
        },
        Diagnostic {
            code: DiagnosticCode::SyntaxError,
            severity: Severity::Error,
            range: NodeRange { start: 8, end: 9 },
            message: "syntax error".to_string(),
            related: Vec::new(),
        },
    ];

    normalize_diagnostics(&mut diagnostics);

    assert_eq!(diagnostics.len(), 3);
    assert_eq!(diagnostics[0].range, range);
    assert_eq!(diagnostics[0].message, "unresolved name `f`");
    assert_eq!(diagnostics[0].related.len(), 2);
    assert_eq!(diagnostics[1].range, range);
    assert_eq!(diagnostics[1].message, "unresolved name `g`");
    assert_eq!(diagnostics[2].range, NodeRange { start: 8, end: 9 });
}

/// Different conflict names at the same contract range retain separate
/// messages and their own related declaration links.
#[test]
fn same_range_conflicts_keep_distinct_messages_and_links() {
    let source = r#"
        contract A { function f() public virtual {} function g() public virtual {} }
        contract B { function f() public virtual {} function g() public virtual {} }
        contract C is A, B {}
    "#;
    let (db, file) = make_test_db(source);
    let found = diagnostics(&db, file);
    let conflicts: Vec<_> = found
        .iter()
        .filter(|diagnostic| diagnostic.code == DiagnosticCode::OverrideConflict)
        .collect();

    let mut messages: Vec<_> = conflicts.iter().map(|d| d.message.as_str()).collect();
    messages.sort();
    assert_eq!(conflicts.len(), 2, "{messages:?}");
    assert!(messages[0].contains("`f`") && messages[1].contains("`g`"), "{messages:?}");
    for conflict in conflicts {
        assert_eq!(conflict.related.len(), 2, "{:?}", conflict.related);
    }
}

/// Referencing a conflicted member reports at the use site and links
/// every conflicting base declaration.
#[test]
fn conflicted_member_use_links_every_base_declaration() {
    let source = r#"
        contract A { function f() public virtual {} }
        contract B { function f() public virtual {} }
        contract C is A, B { function probe(C c) external { c.f(); } }
    "#;
    let (db, file) = make_test_db(source);
    let found = diagnostics(&db, file);
    let ambiguity = found
        .iter()
        .find(|diagnostic| diagnostic.code == DiagnosticCode::AmbiguousMember)
        .expect("the conflicted member use should be reported");

    let use_offset = source.find("c.f()").unwrap() as u32;
    assert!(
        ambiguity.range.start <= use_offset && use_offset < ambiguity.range.end,
        "the use-site diagnostic should cover `c.f()`"
    );
    assert_eq!(ambiguity.related.len(), 2);
}

/// A bare-name use inside the contract links every conflicting base
/// declaration, including an `external` one that the Direct path hides.
#[test]
fn bare_conflicted_call_links_external_base_declarations() {
    let source = r#"
        interface I { function f() external; }
        contract A { function f() public virtual {} }
        contract C is I, A { function probe() external { f(); } }
    "#;
    let (db, file) = make_test_db(source);
    let found = diagnostics(&db, file);
    let ambiguity = found
        .iter()
        .find(|diagnostic| diagnostic.code == DiagnosticCode::AmbiguousMember)
        .expect("the bare conflicted call should be reported");
    assert_eq!(ambiguity.related.len(), 2, "{:?}", ambiguity.related);
}

/// Identical overload-conflict messages coalesce while retaining the
/// related links for every conflicting declaration.
#[test]
fn overloaded_conflicts_on_one_name_merge_related_links() {
    let source = r#"
        contract A { function f(uint) public virtual {} function f(bool) public virtual {} }
        contract B { function f(uint) public virtual {} function f(bool) public virtual {} }
        contract C is A, B {}
    "#;
    let (db, file) = make_test_db(source);
    let found = diagnostics(&db, file);
    let conflicts: Vec<_> = found
        .iter()
        .filter(|diagnostic| diagnostic.code == DiagnosticCode::OverrideConflict)
        .collect();
    assert_eq!(conflicts.len(), 1, "{:?}", conflicts.iter().map(|d| &d.message).collect::<Vec<_>>());
    assert_eq!(conflicts[0].message, "derived contract must override `f`: two or more bases define it with the same parameter types");
    assert_eq!(conflicts[0].related.len(), 4, "{:?}", conflicts[0].related);
    assert!(conflicts[0].related.iter().all(|link| link.message == "declared in `A`" || link.message == "declared in `B`"));
}

#[test]
fn redeclaration_is_reported_at_the_rejected_name_with_the_previous_declaration() {
    let source = "contract C { uint x; uint x; }";
    let (db, file) = make_test_db(source);
    let found = diagnostics(&db, file);
    let redeclared: Vec<_> = found
        .iter()
        .filter(|diagnostic| diagnostic.code == DiagnosticCode::Redeclaration)
        .collect();

    assert_eq!(redeclared.len(), 1);
    assert_eq!(
        redeclared[0].range.start as usize,
        source.rfind('x').unwrap()
    );
    assert_eq!(redeclared[0].related.len(), 1);
    assert_eq!(
        redeclared[0].related[0].range.start as usize,
        source.find('x').unwrap()
    );
    assert_eq!(
        redeclared[0].related[0].message,
        "previous declaration is here"
    );
}

#[test]
fn ambiguous_overload_formats_each_signature_as_plain_text() {
    let source = r#"
        contract C {
            function f(uint8 a) public pure returns (uint8) { return a; }
            function f(uint16 a) public pure returns (uint16) { return a; }
            function g() external { f(1); }
        }
    "#;
    let (db, file) = make_test_db(source);
    let found = diagnostics(&db, file);
    let ambiguity = found
        .iter()
        .find(|diagnostic| diagnostic.code == DiagnosticCode::AmbiguousOverload)
        .expect("the call should be ambiguous");

    assert_eq!(ambiguity.message, "ambiguous overload (2 candidates)");
    assert_eq!(ambiguity.related.len(), 2);
    assert!(ambiguity.related.iter().any(|link| {
        link.message == "function f(uint8 a) public pure returns (uint8) — Test.sol:3"
    }));
    assert!(ambiguity.related.iter().any(|link| {
        link.message == "function f(uint16 a) public pure returns (uint16) — Test.sol:4"
    }));
    assert!(!ambiguity.message.contains("```"));
    assert!(!ambiguity.message.contains("publicpure"));
}

#[test]
fn access_failure_includes_declaration_location() {
    let source = r#"
        contract A { function internalOnly() internal {} }
        contract C {
            function check(A a) external { a.internalOnly(); }
        }
    "#;
    let (db, file) = make_test_db(source);
    let found = diagnostics(&db, file);
    let failure = found
        .iter()
        .find(|diagnostic| diagnostic.code == DiagnosticCode::UnresolvedMember)
        .expect("inaccessible member should be diagnosed");

    assert!(failure.message.contains("internalOnly"));
    assert!(failure.message.contains("internal"));
    assert_eq!(failure.related.len(), 1);
    assert_eq!(
        failure.related[0].range.start as usize,
        source.find("internalOnly").unwrap()
    );
    assert!(failure.related[0].message.contains("internalOnly"));
}

#[test]
fn collects_incomplete_member_expressions() {
    let source = "contract C { function f() public { msg.; } }";
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_solidity::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let mut diagnostics = Vec::new();

    collect_parse_errors(tree.root_node(), source, &mut diagnostics);

    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message == "expected a member name after `.`" })
    );
}
