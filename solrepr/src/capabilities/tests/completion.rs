use crate::capabilities::completion::*;
use crate::hir::resolver::Resolver;
use crate::hir::tests::helper::*;
use crate::ir::def_map::DefId;
use crate::salsa::hir_db::owner_at;
use crate::salsa::{File, HirDb, RootDb, Db};
use lsp_types::{CompletionItem as LspCompletionItem, CompletionTriggerKind};

/// Drive the dot-completion path the way `completion()` does, using a
/// literal marker string to locate the cursor.
fn dot_labels(db: &Db, file: File, marker: &str) -> Vec<String> {
    let text = db.text(file);
    let offset = (text.find(marker).expect("marker") + marker.len()) as u32;
    let node = db.named_node_at(file, offset).expect("node at marker");
    let owner = owner_at(db, file, offset).unwrap();
    let resolver = Resolver::build(db, owner);
    let kind = detect_ctx(&node, offset, CompletionTriggerKind::INVOKED, None).expect("owner");
    let CompletionContextKind::DotAccess { receiver_node } = kind else {
        panic!("expected dot access at {marker:?}");
    };
    complete_dot(db, &resolver, owner, &receiver_node)
        .into_iter()
        .map(|item| item.label)
        .collect()
}

const FIXTURE: &str = r#"
    interface IERC20 { function transfer(address to, uint256 v) external returns (bool); }
    interface IERC1363 is IERC20 { function transferAndCall(address to, uint256 v) external returns (bool); }
    library SafeERC20 {
        function safeTransfer(IERC20 token, address to, uint256 v) internal {}
        function transferAndCallRelaxed(IERC1363 token, address to, uint256 v, bytes memory d) internal {}
    }
    contract C {
        using SafeERC20 for IERC20;
        function p1(IERC20 alpha) external { alpha.; }
        function p2(IERC20 beta) external { IERC20(beta).; }
        function p3() external { IERC20.transfer.; }
        function p4(IERC20 delta) external { IERC20(delta).transfer.; }
        function p5(IERC20 epsilon) external { epsilon.safeTransfer.; }
    }
"#;

#[test]
fn dot_completion_on_interface_value() {
    let (db, file) = make_test_db(FIXTURE);
    let labels = dot_labels(&db, file, "alpha.");
    assert!(
        labels.iter().any(|l| l.starts_with("transfer")),
        "got {labels:?}"
    );
    assert!(
        labels.iter().any(|l| l.starts_with("safeTransfer")),
        "got {labels:?}"
    );
    // A library function whose first parameter is a *subtype* must not be
    // attached to the supertype receiver.
    assert!(
        !labels
            .iter()
            .any(|l| l.starts_with("transferAndCallRelaxed")),
        "got {labels:?}"
    );
}

#[test]
fn dot_completion_on_cast_receiver() {
    let (db, file) = make_test_db(FIXTURE);
    let labels = dot_labels(&db, file, "IERC20(beta).");
    assert!(
        labels.iter().any(|l| l.starts_with("transfer")),
        "got {labels:?}"
    );
    assert!(
        labels.iter().any(|l| l.starts_with("safeTransfer")),
        "got {labels:?}"
    );
    assert!(
        !labels
            .iter()
            .any(|l| l.starts_with("transferAndCallRelaxed")),
        "got {labels:?}"
    );
}

#[test]
fn dot_completion_on_types_function() {
    let (db, file) = make_test_db(FIXTURE);
    let labels = dot_labels(&db, file, "IERC20.transfer.");
    assert!(labels.iter().any(|l| l == "selector"), "got {labels:?}");
}

#[test]
fn dot_completion_on_external_function_value() {
    let (db, file) = make_test_db(FIXTURE);
    let labels = dot_labels(&db, file, "IERC20(delta).transfer.");
    assert!(labels.iter().any(|l| l == "selector"), "got {labels:?}");
    assert!(labels.iter().any(|l| l == "address"), "got {labels:?}");
}

#[test]
fn dot_completion_on_bound_internal_function_is_empty() {
    let (db, file) = make_test_db(FIXTURE);
    let labels = dot_labels(&db, file, "epsilon.safeTransfer.");
    assert!(labels.is_empty(), "got {labels:?}");
}

/// A `using`-attached function's label carries the *unbound* parameter list —
/// the member access already binds the receiver parameter.
#[test]
fn dot_completion_elides_the_bound_receiver_parameter() {
    let (db, file) = make_test_db(FIXTURE);
    let text = db.text(file);
    let offset = (text.find("alpha.").expect("marker") + "alpha.".len()) as u32;
    let node = db.named_node_at(file, offset).expect("node at marker");
    let owner = owner_at(&db, file, offset).unwrap();
    let resolver = Resolver::build(&db, owner);
    let kind = detect_ctx(&node, offset, CompletionTriggerKind::INVOKED, None).expect("owner");
    let CompletionContextKind::DotAccess { receiver_node } = kind else {
        panic!("expected dot access");
    };
    let items = complete_dot(&db, &resolver, owner, &receiver_node);

    let attached = items
        .iter()
        .find(|item| item.label.starts_with("safeTransfer"))
        .expect("attached `safeTransfer` should be offered");
    assert_eq!(attached.label, "safeTransfer(address, uint256)");
    assert_eq!(
        attached
            .label_details
            .as_ref()
            .and_then(|details| details.description.as_deref()),
        Some("function (address, uint256) "),
        "the bound `IERC20 token` receiver must not appear in the signature"
    );
}

/// Overloads stay distinct items — the signature is part of the label.
#[test]
fn dot_completion_keeps_overloads_apart() {
    let (db, file) = make_test_db(
        r#"
        contract C {
            function f(uint8 a) external {}
            function f(uint256 a) external {}
            function probe(C c) external { c.; }
        }
        "#,
    );
    let labels = dot_labels(&db, file, "c.");
    assert!(labels.iter().any(|l| l == "f(uint8)"), "got {labels:?}");
    assert!(labels.iter().any(|l| l == "f(uint256)"), "got {labels:?}");
}

/// Bare-name completion inside a derived contract enumerates the inherited
/// members that named lookup would resolve — internal/public — and omits the
/// ones it would deny: base `private` and `external` declarations (solc
/// reports both as "Undeclared identifier").
#[test]
fn bare_completion_lists_accessible_inherited_members() {
    let (db, file) = make_test_db(
        r#"
        contract A {
            uint internal n;
            uint private p;
            function inter() internal {}
            function pub_() public {}
            function ext() external {}
        }
        contract C is A {
            function probe() external { marker }
        }
        "#,
    );
    let text = db.text(file);
    let offset = (text.find("marker").expect("marker") + "marker".len()) as u32;
    let owner = owner_at(&db, file, offset).unwrap();
    let resolver = Resolver::build(&db, owner);
    let labels: Vec<String> = complete_expr(&db, &resolver, owner, offset)
        .into_iter()
        .map(|item| item.label)
        .collect();

    for offered in ["n", "inter()", "pub_()"] {
        assert!(
            labels.iter().any(|l| l == offered),
            "inherited `{offered}` should be offered; got {labels:?}"
        );
    }
    for denied in ["p", "ext"] {
        assert!(
            !labels
                .iter()
                .any(|l| l == denied || *l == format!("{denied}()")),
            "inaccessible `{denied}` must not be offered"
        );
    }
}

// MARK: Braced groups — named arguments and call options

/// Drive the braced-group path the way `completion()` does, using a comment
/// marker to place the cursor. A comment is a tree-sitter *extra*, so it does
/// not disturb the group's shape.
fn group_items(db: &Db, file: File, marker: &str) -> Vec<LspCompletionItem> {
    let text = db.text(file);
    let offset = (text.find(marker).expect("marker") + marker.len()) as u32;
    let node = db.named_node_at(file, offset).expect("node at marker");
    let owner = owner_at(db, file, offset).unwrap();
    match detect_ctx(&node, offset, CompletionTriggerKind::INVOKED, None)
        .expect("a braced group at the marker")
    {
        CompletionContextKind::BracedGroup {
            kind,
            callee_node,
            existing,
            insert_separator,
        } => complete_braced_group(
            db,
            owner,
            kind,
            &callee_node,
            &existing,
            insert_separator,
        ),
        other => panic!("expected a braced group, got {other:?}"),
    }
}

fn group_labels(db: &Db, file: File, marker: &str) -> Vec<String> {
    group_items(db, file, marker)
        .into_iter()
        .map(|item| item.label)
        .collect()
}

/// Whether the cursor at `marker` is a braced-group key position.
fn is_group_key(db: &Db, file: File, marker: &str) -> bool {
    let text = db.text(file);
    let offset = (text.find(marker).expect("marker") + marker.len()) as u32;
    let node = db.named_node_at(file, offset).expect("node at marker");
    matches!(
        detect_ctx(&node, offset, CompletionTriggerKind::INVOKED, None),
        Some(CompletionContextKind::BracedGroup { .. })
    )
}

const GROUP_FIXTURE: &str = r#"
    struct S { uint x; uint y; }
    interface I { function t(address to, uint amount) external; }
    library L { function h(uint first, uint second) internal {} }
    library L2 { function wrapped(I self, address to, uint amount) internal {} }
    contract B { constructor(uint seed) {} }
    contract C {
        using L2 for I;
        function g(uint alpha, uint beta) public payable {}
        function probe() external {
            g({ /*empty*/ });
            g({ alpha: 1, /*second*/ });
            g({ alpha: 1, beta: 2, /*full*/ });
            g({ alpha: /*value*/ 1 });
            S({ /*struct*/ });
            L.h({ /*lib*/ });
            this.g({ /*this*/ });
            this.g{ /*options*/ }(1, 2);
            this.g{ value: 1, /*optionSecond*/ }(1, 2);
            this.g{ value: 1, gas: 1, /*optionFull*/ }(1, 2);
            this.g{ val/*editingOption*/: 1 }(1, 2);
            B b = new B{ /*new*/ }(1);
            I i;
            i.t({ /*iface*/ });
            i.wrapped({ /*bound*/ });
        }
    }
"#;

/// A named-argument key offers the callee's parameter names.
#[test]
fn named_arg_keys_are_the_callee_parameters() {
    let (db, file) = make_test_db(GROUP_FIXTURE);
    let labels = group_labels(&db, file, "/*empty*/");
    assert!(labels.contains(&"alpha".to_string()), "got {labels:?}");
    assert!(labels.contains(&"beta".to_string()), "got {labels:?}");
}

/// The key is inserted with its separator and a snippet stop, since the cursor
/// sits before the `:` and the value is what comes next.
#[test]
fn named_arg_keys_insert_the_separator() {
    let (db, file) = make_test_db(GROUP_FIXTURE);
    let item = group_items(&db, file, "/*empty*/")
        .into_iter()
        .find(|item| item.label == "alpha")
        .expect("`alpha` should be offered");
    assert_eq!(item.insert_text.as_deref(), Some("alpha: $0"));
    assert_eq!(
        item.label_details.and_then(|details| details.description),
        Some("uint256".to_string()),
        "the key carries the parameter's declared type"
    );
}

/// A key that has already been written is not offered again.
#[test]
fn named_arg_keys_skip_the_ones_already_written() {
    let (db, file) = make_test_db(GROUP_FIXTURE);
    assert_eq!(group_labels(&db, file, "/*second*/"), ["beta"]);
    assert!(group_labels(&db, file, "/*full*/").is_empty());
}

/// After the `:` the cursor is in the value, which is an ordinary expression.
#[test]
fn a_value_position_is_not_a_key_position() {
    let (db, file) = make_test_db(GROUP_FIXTURE);
    assert!(!is_group_key(&db, file, "/*value*/"));
}

/// `S({ … })` names a struct's fields.
#[test]
fn named_arg_keys_cover_a_struct_construction() {
    let (db, file) = make_test_db(GROUP_FIXTURE);
    assert_eq!(group_labels(&db, file, "/*struct*/"), ["x", "y"]);
}

/// A member callee works the same as a bare one.
#[test]
fn named_arg_keys_cover_member_callees() {
    let (db, file) = make_test_db(GROUP_FIXTURE);
    for (marker, expected) in [
        ("/*lib*/", ["first", "second"]),
        ("/*this*/", ["alpha", "beta"]),
        ("/*iface*/", ["to", "amount"]),
    ] {
        assert_eq!(
            group_labels(&db, file, marker),
            expected,
            "`{marker}` should offer the callee's parameters"
        );
    }
}

/// A `using`-attached function's receiver parameter is implicit, so it has no
/// key.
#[test]
fn named_arg_keys_skip_a_bound_receiver() {
    let (db, file) = make_test_db(GROUP_FIXTURE);
    assert_eq!(group_labels(&db, file, "/*bound*/"), ["to", "amount"]);
}

/// Call options are `value`/`gas` for an external call, and `value`/`salt` for
/// a contract creation — `gas` is refused on `new`.
#[test]
fn call_option_keys_depend_on_the_callee() {
    let (db, file) = make_test_db(GROUP_FIXTURE);
    assert_eq!(group_labels(&db, file, "/*options*/"), ["value", "gas"]);
    assert_eq!(group_labels(&db, file, "/*optionSecond*/"), ["gas"]);
    assert!(group_labels(&db, file, "/*optionFull*/").is_empty());
    assert_eq!(group_labels(&db, file, "/*new*/"), ["value", "salt"]);
}

#[test]
fn editing_a_call_option_key_does_not_duplicate_its_colon() {
    let (db, file) = make_test_db(GROUP_FIXTURE);
    let item = group_items(&db, file, "/*editingOption*/")
        .into_iter()
        .find(|item| item.label == "value")
        .expect("value remains a candidate while editing its key");
    assert_eq!(item.insert_text.as_deref(), Some("value"));
}

// MARK: Partial and broken source

/// The state a user is in the moment they type the `,`: the closing brace may
/// not exist yet, so the parser folds the statement into an `ERROR`. The callee
/// is still inferred — `infer_call` infers it *before* the arguments, and
/// `infer_all`'s sweep types it even when no call was formed.
const PARTIAL_FIXTURE: &str = r#"
    contract C {
        function g(uint alpha, uint beta) public payable {}
        function probe() external {
            g({ alpha: 1, /*unclosed*/ );
            g({ /*opened*/ );
            this.g{ /*opts*/ (1, 2);
        }
    }
"#;

#[test]
fn named_arg_keys_survive_an_unclosed_brace() {
    let (db, file) = make_test_db(PARTIAL_FIXTURE);
    assert_eq!(
        group_labels(&db, file, "/*unclosed*/"),
        ["beta"],
        "the callee is still resolved with no closing brace"
    );
    let mut opened = group_labels(&db, file, "/*opened*/");
    opened.sort();
    assert_eq!(opened, ["alpha", "beta"]);
}

#[test]
fn call_option_keys_survive_an_unclosed_brace() {
    let (db, file) = make_test_db(PARTIAL_FIXTURE);
    assert_eq!(group_labels(&db, file, "/*opts*/"), ["value", "gas"]);
}

const EXTRA_CALLEE_FIXTURE: &str = r#"
    contract B { constructor(uint alpha, uint beta) {} }
    contract C {
        event Ev(uint alpha, uint beta);
        error Er(uint alpha, uint beta);
        modifier m(uint alpha, uint beta) { _; }
        function f(uint alpha, uint beta) public m({ /*modifier*/ }) {}
        function probe() external {
            emit Ev({ /*emit*/ });
            revert Er({ /*revert*/ });
            new B({ /*newNamed*/ });
        }
    }
"#;

#[test]
fn named_group_callees_include_emit_revert_new_and_modifier() {
    let (db, file) = make_test_db(EXTRA_CALLEE_FIXTURE);
    for marker in ["/*emit*/", "/*revert*/", "/*newNamed*/", "/*modifier*/"] {
        assert_eq!(
            group_labels(&db, file, marker),
            ["alpha", "beta"],
            "failed to complete the braced group at {marker}"
        );
    }
}

#[test]
fn nested_named_arguments_do_not_pollute_outer_existing_keys() {
    let (db, file) = make_test_db(
        r#"
        contract C {
            function inner(uint x) internal pure returns (uint) { return x; }
            function outer(uint a, uint x, uint y) internal pure {}
            function probe() external {
                outer({ a: inner({ x: 1 }), /*outer*/ });
            }
        }
        "#,
    );
    assert_eq!(group_labels(&db, file, "/*outer*/"), ["x", "y"]);
}

#[test]
fn delimiters_in_strings_and_comments_do_not_change_the_current_key() {
    let (db, file) = make_test_db(
        r#"
        contract C {
            function outer(string memory a, uint x, uint y) internal pure {}
            function probe() external {
                outer({ a: "comma, colon: brace }", /* : , { } */ });
            }
        }
        "#,
    );
    assert_eq!(group_labels(&db, file, "/* : , { } */"), ["x", "y"]);
}

#[test]
fn external_function_pointer_options_are_offered() {
    let (db, file) = make_test_db(
        r#"
        contract C {
            function() external payable fp;
            function probe() external { fp{ /*fnptr*/ }(); }
        }
        "#,
    );
    assert_eq!(group_labels(&db, file, "/*fnptr*/"), ["value", "gas"]);
}

#[test]
fn editing_a_key_before_its_colon_does_not_insert_another_colon() {
    let (db, file) = make_test_db(
        r#"
        contract C {
            function g(uint256 alpha) public {}
            function probe() external { g({ alp/*editing*/: 1 }); }
        }
        "#,
    );
    let item = group_items(&db, file, "/*editing*/")
        .into_iter()
        .find(|item| item.label == "alpha")
        .expect("alpha remains a candidate while editing its name");
    assert_eq!(item.insert_text.as_deref(), Some("alpha"));
}

#[test]
fn incomplete_pair_does_not_discard_completed_named_pairs() {
    let (db, file) = make_test_db(
        r#"
        contract C {
            function g(uint256 alpha, uint256 beta) public {}
            function probe() external { g({ alpha: 1, beta: }); }
        }
        "#,
    );
    let def = find_function(&db, file, "probe");
    let body = db.body_map(crate::hir::body_map::BodyOwnerId::Function(match def {
        DefId::Function(id) => id,
        _ => unreachable!(),
    }));
    let pairs = body
        .syntax_store
        .exprs
        .iter()
        .find_map(|(_, expr)| match expr {
            crate::hir::exprs::Expr::NamedArgs(named) => Some(named),
            _ => None,
        })
        .expect("the braced argument should still lower");
    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[0].0.as_str(), "alpha");
}

#[test]
fn incomplete_call_option_does_not_discard_completed_options() {
    let (db, file) = make_test_db(
        r#"
        contract C {
            function g() public payable {}
            function probe() external { this.g{ value: 1, gas: }(); }
        }
        "#,
    );
    let def = find_function(&db, file, "probe");
    let body = db.body_map(crate::hir::body_map::BodyOwnerId::Function(match def {
        DefId::Function(id) => id,
        _ => unreachable!(),
    }));
    let options = body
        .syntax_store
        .exprs
        .iter()
        .find_map(|(_, expr)| match expr {
            crate::hir::exprs::Expr::CallOptions { options, .. } => Some(options),
            _ => None,
        })
        .expect("the call-options group should still lower");
    assert_eq!(options.len(), 1);
    assert_eq!(options[0].0.as_str(), "value");
}
