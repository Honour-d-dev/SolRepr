use super::*;

/// solc's definition-shape rules: a bodyless function or modifier must be
/// `virtual` (except interface functions, which are implicitly virtual), and
/// library/free functions must have an implementation.
#[test]
fn bodyless_definitions_follow_solc() {
    let (db, file) = make_test_db(
        r#"
            abstract contract C { function cf() public; modifier cm(); }
            abstract contract V { function vf() public virtual; modifier vm() virtual; }
            interface I { function ig() external; }
            library L { function lh() internal; modifier ln(); }
            function free();
            "#,
    );

    for name in ["cf"] {
        assert_eq!(
            codes(&named_function_inference(&db, file, name)),
            vec![DiagnosticCode::MissingVirtual],
            "{name}",
        );
    }
    for name in ["cm", "ln"] {
        assert_eq!(
            codes(&named_modifier_inference(&db, file, name)),
            vec![DiagnosticCode::MissingVirtual],
            "{name}",
        );
    }
    for name in ["lh", "free"] {
        assert_eq!(
            codes(&named_function_inference(&db, file, name)),
            vec![DiagnosticCode::MissingImplementation],
            "{name}",
        );
    }
    for name in ["vf", "ig"] {
        assert!(
            codes(&named_function_inference(&db, file, name)).is_empty(),
            "{name}",
        );
    }
    assert!(
        codes(&named_modifier_inference(&db, file, "vm")).is_empty(),
        "vm",
    );
}

/// solc's placement rules: an interface can't declare modifiers at all, and
/// `virtual` is never allowed in a library.
#[test]
fn declaration_placement_follows_solc() {
    let (db, file) = make_test_db(
        r#"
            interface I { modifier body(); modifier impl_() { _; } }
            library L {
                modifier vm() virtual { _; }
                function vf() internal virtual {}
                function vfBodyless() internal virtual;
            }
            "#,
    );

    let modifier_codes = |name: &str| codes(&named_modifier_inference(&db, file, name));
    let function_codes = |name: &str| codes(&named_function_inference(&db, file, name));

    // 6408 + 8063, exactly like solc.
    assert_eq!(
        modifier_codes("body"),
        vec![DiagnosticCode::InferenceFailure, DiagnosticCode::MissingVirtual],
    );
    // 6408 only: it has a body.
    assert_eq!(
        modifier_codes("impl_"),
        vec![DiagnosticCode::InferenceFailure],
    );
    // 3275.
    assert_eq!(modifier_codes("vm"), vec![DiagnosticCode::VirtualInLibrary]);
    // 7801.
    assert_eq!(function_codes("vf"), vec![DiagnosticCode::VirtualInLibrary]);
    // 7801 + "must be implemented".
    assert_eq!(
        function_codes("vfBodyless"),
        vec![DiagnosticCode::VirtualInLibrary, DiagnosticCode::MissingImplementation],
    );
}

/// solc's `_` rule: a modifier body must contain the placeholder as a
/// standalone statement.
#[test]
fn modifier_body_requires_the_placeholder() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                modifier empty() { }
                modifier ok() { _; }
                modifier nested(bool x) { if (x) { _; } }
                modifier afterReturn() { return; _; }
                modifier declaredName() { uint _ = 1; }
                modifier spaced() { _ ; }
                modifier commented() { _ /* c */; }
                modifier inString() { string memory s = "_;"; }
                modifier inComment() { // _;
                }
            }
            interface I { modifier iface() { } }
            "#,
    );

    let codes_of = |name: &str| codes(&named_modifier_inference(&db, file, name));

    assert_eq!(codes_of("empty"), vec![DiagnosticCode::InferenceFailure]);
    assert!(codes_of("ok").is_empty(), "{:?}", codes_of("ok"));
    assert!(codes_of("nested").is_empty(), "{:?}", codes_of("nested"));
    assert!(
        codes_of("afterReturn").is_empty(),
        "{:?}",
        codes_of("afterReturn")
    );
    // `_` as a declaration name is reserved, not the placeholder.
    assert_eq!(
        codes_of("declaredName"),
        vec![DiagnosticCode::InferenceFailure]
    );
    // Whitespace and comments around `_` are fine.
    assert!(codes_of("spaced").is_empty(), "{:?}", codes_of("spaced"));
    assert!(codes_of("commented").is_empty(), "{:?}", codes_of("commented"));
    // `_` inside a string or a comment is not the placeholder.
    assert_eq!(
        codes_of("inString"),
        vec![DiagnosticCode::InferenceFailure]
    );
    assert_eq!(
        codes_of("inComment"),
        vec![DiagnosticCode::InferenceFailure]
    );
    // The placeholder error points at the body, 6408 at the declaration.
    assert_eq!(
        codes_of("iface"),
        vec![DiagnosticCode::InferenceFailure, DiagnosticCode::InferenceFailure]
    );
}


