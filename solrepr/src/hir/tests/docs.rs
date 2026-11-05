use crate::hir::docs::{DocEntry, format_doc, normalize_doc, parse_doc};
use crate::hir::tests::helper::{find_function, make_test_db};
use crate::salsa::HirDb;

#[test]
fn normalizes_doc_comment_prefixes_and_code_fences() {
    let raw = "/// @notice Example\n/// @param value The value\n/// ```solidity\n///     uint256 value;\n/// ```";
    let normalized = normalize_doc(raw);
    assert_eq!(
        normalized,
        "\n**Notice:** Example\n- **`value`**: The value\n```solidity\n    uint256 value;\n```"
    );
}

#[test]
fn merge_missing_fills_only_absent_tags() {
    let own = parse_doc("/// @notice own\n/// @param a own-a");
    let base = parse_doc(
        "/// @notice base\n/// @param a base-a\n/// @param b base-b\n/// @return base-r",
    );
    let mut merged = own;
    merged.merge_missing(&base);
    let doc = format_doc(&merged);
    assert!(doc.contains("**Notice:** own"), "got {doc:?}");
    assert!(!doc.contains("**Notice:** base"), "got {doc:?}");
    assert!(doc.contains("- **`a`**: own-a"), "got {doc:?}");
    assert!(!doc.contains("base-a"), "got {doc:?}");
    assert!(doc.contains("- **`b`**: base-b"), "got {doc:?}");
    assert!(doc.contains("**Returns** base-r"), "got {doc:?}");
}

#[test]
fn merge_missing_always_appends_body_text() {
    let mut own = parse_doc("/// @param a own-a");
    own.merge_missing(&parse_doc("/// base body text\n/// @return r"));
    assert!(
        own.entries
            .iter()
            .any(|e| matches!(e, DocEntry::Text(t) if t == "base body text")),
        "got {:?}",
        own.entries
    );

    // Even with its own body text, the base's text is appended.
    let mut own = parse_doc("/// own body");
    own.merge_missing(&parse_doc("/// base body text"));
    let text: Vec<_> = own
        .entries
        .iter()
        .filter_map(|e| match e {
            DocEntry::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, ["own body", "base body text"], "got {text:?}");
}

#[test]
fn only_doc_comments_produce_docs() {
    let (db, file) = make_test_db(
        r#"
        // a plain line comment
        /* a plain block comment */
        function plain() public {}
        /// a doc line
        // a plain comment between doc and decl
        function documented() public {}
        /** a doc block */
        function blocked() public {}
        "#,
    );
    assert!(db.docs(find_function(&db, file, "plain")).is_none());
    let doc = db
        .docs(find_function(&db, file, "documented"))
        .expect("docs");
    assert!(doc.contains("a doc line"), "got {doc:?}");
    assert!(!doc.contains("plain comment"), "got {doc:?}");
    let doc = db.docs(find_function(&db, file, "blocked")).expect("docs");
    assert!(doc.contains("a doc block"), "got {doc:?}");
}

#[test]
fn inheritdoc_fills_missing_tags() {
    // find_function returns the *last* same-named function — D goes last.
    let (db, file) = make_test_db(
        r#"
        contract B {
            /// @notice Base notice.
            /// @param x The value.
            /// @return y The result.
            function f(uint256 x) public virtual returns (uint256 y) {}
        }
        contract D is B {
            /// @notice Own notice.
            /// @inheritdoc B
            function f(uint256 x) public virtual {}
        }
        "#,
    );
    let doc = db.docs(find_function(&db, file, "f")).expect("docs");
    assert!(doc.contains("**Notice:** Own notice."), "got {doc:?}");
    assert!(!doc.contains("Base notice"), "got {doc:?}");
    assert!(doc.contains("- **`x`**: The value"), "got {doc:?}");
    assert!(doc.contains("**Returns**"), "got {doc:?}");
}

#[test]
fn inheritdoc_picks_the_matching_overload() {
    let (db, file) = make_test_db(
        r#"
        contract B {
            /// @notice Address overload.
            function f(address a) public virtual {}
            /// @notice Uint overload.
            function f(uint256 x) public virtual {}
        }
        contract D is B {
            /// @inheritdoc B
            function f(uint256 x) public virtual {}
        }
        "#,
    );
    let doc = db.docs(find_function(&db, file, "f")).expect("docs");
    assert!(doc.contains("Uint overload"), "got {doc:?}");
    assert!(!doc.contains("Address overload"), "got {doc:?}");
}

#[test]
fn inheritdoc_chains_through_bases() {
    let (db, file) = make_test_db(
        r#"
        contract A {
            /// @notice Root notice.
            /// @dev Root dev.
            function f() public virtual {}
        }
        contract B is A {
            /// @inheritdoc A
            function f() public virtual {}
        }
        contract D is B {
            /// @inheritdoc B
            function f() public virtual {}
        }
        "#,
    );
    let doc = db.docs(find_function(&db, file, "f")).expect("docs");
    assert!(doc.contains("**Notice:** Root notice."), "got {doc:?}");
    assert!(doc.contains("**Dev note:** Root dev."), "got {doc:?}");
}

#[test]
fn inheritdoc_unknown_base_keeps_own_docs() {
    let (db, file) = make_test_db(
        r#"
        contract B {
            /// @notice Base.
            function f() public virtual {}
        }
        contract D is B {
            /// @notice Own.
            /// @inheritdoc Missing
            function f() public virtual {}
        }
        "#,
    );
    let doc = db.docs(find_function(&db, file, "f")).expect("docs");
    assert!(doc.contains("**Notice:** Own."), "got {doc:?}");
    assert!(!doc.contains("Base."), "got {doc:?}");
}

#[test]
fn doc_tags_roundtrip() {
    // A def with no @inheritdoc still parses and formats normally.
    let (db, file) = make_test_db(
        r#"
        contract C {
            /// @param a first
            function f(uint256 a) public {}
        }
        "#,
    );
    let doc = db.docs(find_function(&db, file, "f")).expect("docs");
    assert!(doc.contains("- **`a`**: first"), "got {doc:?}");
}
