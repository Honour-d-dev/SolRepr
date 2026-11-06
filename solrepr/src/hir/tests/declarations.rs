use super::*;

use crate::ast::kinds::NodeKind;
use crate::ast::{Enum, EnumId, Event, EventId, Struct, StructId};
use crate::capabilities::diagnostics::diagnostics;
use crate::hir::body_map::BodyOwnerId;

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



/// Locals become visible in declaration order, not by byte offset.
#[test]
fn locals_follow_declaration_order() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                function after() external { uint x = 1; x; }
                function before() external { uint x = x; }
            }
            "#,
    );

    assert!(
        codes(&named_function_inference(&db, file, "after")).is_empty(),
        "a local is visible after its declaration"
    );
    assert!(
        codes(&named_function_inference(&db, file, "before"))
            .contains(&DiagnosticCode::UnresolvedName),
        "a local is not visible in its own initializer"
    );
}

/// A declaration that is not yet visible is skipped, so the initializer sees
/// the next binding outward — matching solc's shadowing rule.
#[test]
fn initializer_sees_an_outer_binding() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                uint s;
                function outer_local() external { uint x = 1; { uint x = x; } }
                function state_var() external { uint s = s; }
                function param(uint p) external { { uint p = p; } }
            }
            "#,
    );

    for name in ["outer_local", "state_var", "param"] {
        assert!(
            codes(&named_function_inference(&db, file, name)).is_empty(),
            "{name} should resolve its initializer to the outer binding",
        );
    }
}

/// Diagnostics anchor by id, so reflow outside a function leaves its
/// `Inference` output identical even though the query re-runs.
#[test]
fn inference_is_stable_under_reflow() {
    let (mut db, file) = make_test_db(
        r#"contract C { function f() external { uint x = "bad"; } }"#,
    );
    let DefId::Function(id) = find_function(&db, file, "f") else {
        panic!("fixture function should exist");
    };
    let before = db.inference(DefId::Function(id));
    let before_data = db.function_data(id);
    let before_body = db.body_map(BodyOwnerId::Function(id));

    let path = file.path(&db).to_path_buf();
    db.open_file(
        path,
        r#"contract C {
            function f() external { uint x = "bad"; }
        }"#
        .to_string(),
    );

    let after = db.inference(DefId::Function(id));
    assert!(
        before == after,
        "a reflow outside the function must not change its inference result"
    );
    assert!(
        before_data == db.function_data(id),
        "item content must not change under a reflow"
    );
    assert!(
        before_body == db.body_map(BodyOwnerId::Function(id)),
        "body content must not change under a reflow"
    );

    // Anchors resolve against the *current* positions, not the cached ones.
    let reported = diagnostics(&db, file);
    let mismatch = reported
        .iter()
        .find(|diagnostic| diagnostic.code == DiagnosticCode::TypeMismatch)
        .expect("the type mismatch should still be reported");
    let text = db.text(file);
    assert_eq!(
        &text[mismatch.range.start as usize..mismatch.range.end as usize],
        "\"bad\"",
        "the diagnostic should point at the string literal after the reflow"
    );
}

/// The content builder and the source-map builder must allocate the same ids.
#[test]
fn source_maps_cover_every_allocated_id() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                struct S { uint a; uint b; }
                enum E { A, B }
                event Ev(uint x);
                modifier m() { _; }
                function f(uint a) external { uint x = a + 1; }
                uint public s;
            }
            "#,
    );
    let DefId::Function(id) = find_function(&db, file, "f") else {
        panic!("fixture function should exist");
    };

    let body = db.body_map(BodyOwnerId::Function(id));
    let body_map = db.body_source_map(BodyOwnerId::Function(id));
    for (expr, _) in body.syntax_store.exprs.iter() {
        assert!(body_map.expr_ranges.get(expr).is_some(), "expr {expr:?}");
    }
    for (ty, _) in body.syntax_store.types.iter() {
        assert!(body_map.ty_ref_ranges.get(ty).is_some(), "ty {ty:?}");
    }
    for (local, _) in body.locals.iter() {
        assert!(body_map.local_ranges.get(local).is_some(), "local {local:?}");
    }

    let data = db.function_data(id);
    let item_map = db.source_map(DefId::Function(id));
    for (expr, _) in data.syntax_store.exprs.iter() {
        assert!(item_map.expr_ranges.get(expr).is_some(), "item expr {expr:?}");
    }
    for (ty, _) in data.syntax_store.types.iter() {
        assert!(item_map.ty_ref_ranges.get(ty).is_some(), "item ty {ty:?}");
    }
    for (local, _) in data.parameters.iter() {
        assert!(item_map.local_ranges.get(local).is_some(), "item local {local:?}");
    }

    let root = db.ast(file).root();
    let ast_id_map = db.ast_id_map(file);

    let struct_node = find_first_kind(root.node(), NodeKind::STRUCT_DEFINITION)
        .expect("fixture should declare a struct");
    let struct_id = StructId {
        file,
        id: ast_id_map
            .id_of_node::<Struct>(struct_node)
            .expect("struct should have an AstId"),
    };
    let struct_data = db.struct_data(struct_id);
    let struct_map = db.source_map(DefId::Struct(struct_id));
    for (field, _) in struct_data.fields.iter() {
        assert!(
            struct_map.field_ranges.get(field).is_some(),
            "field {field:?}"
        );
    }

    let enum_node = find_first_kind(root.node(), NodeKind::ENUM_DEFINITION)
        .expect("fixture should declare an enum");
    let enum_id = EnumId {
        file,
        id: ast_id_map
            .id_of_node::<Enum>(enum_node)
            .expect("enum should have an AstId"),
    };
    let enum_data = db.enum_data(enum_id);
    let enum_map = db.source_map(DefId::Enum(enum_id));
    for (variant, _) in enum_data.variants.iter() {
        assert!(
            enum_map.variant_ranges.get(variant).is_some(),
            "variant {variant:?}"
        );
    }

    let event_node = find_first_kind(root.node(), NodeKind::EVENT_DEFINITION)
        .expect("fixture should declare an event");
    let event_id = EventId {
        file,
        id: ast_id_map
            .id_of_node::<Event>(event_node)
            .expect("event should have an AstId"),
    };
    let event_data = db.event_data(event_id);
    let event_map = db.source_map(DefId::Event(event_id));
    for (local, _) in event_data.parameters.iter() {
        assert!(
            event_map.local_ranges.get(local).is_some(),
            "event param {local:?}"
        );
    }

    let DefId::Modifier(modifier_id) = find_modifier(&db, file, "m") else {
        panic!("fixture should declare a modifier");
    };
    let modifier = db.modifier_data(modifier_id);
    assert_eq!(
        modifier.has_body,
        db.source_map(DefId::Modifier(modifier_id))
            .body_range
            .is_some(),
        "`has_body` and `body_range` come from the same node"
    );
}
