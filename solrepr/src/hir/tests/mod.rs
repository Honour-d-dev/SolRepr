use crate::hir::body_map::*;
use crate::hir::exprs::*;
use crate::hir::inference::*;
use crate::hir::item_data::*;
use crate::hir::resolution::*;
use crate::hir::semantic_errors::*;
use crate::hir::types::*;
use crate::ir::def_map::*;
use crate::hir::syntax_store::semantic_at;
use crate::salsa::hir_db::def_name;
use crate::salsa::root_db::RootDb;
use crate::salsa::{HirDb, Db};
use helper::*;

pub mod helper;

mod body_map;
mod builtins;
mod calls;
mod declarations;
mod docs;
mod expressions;
mod inference_types;
mod item_data;
mod member_map;
mod members;
mod overrides;
mod resolver_helpers;
mod resolver_members;
mod resolver_names;
mod signatures;
mod types;

/// A `(source, predicate)` pair for a rejection table.
type Rejects = &'static [(&'static str, fn(&SemanticDiagnostic) -> bool)];

/// The diagnostic codes an owner produced — the `{:?}` payload every
/// rejection test prints on failure.
fn codes(inference: &Inference) -> Vec<DiagnosticCode> {
    inference
        .diagnostics
        .iter()
        .map(SemanticDiagnostic::code)
        .collect()
}

fn expr_results(inference: &Inference) -> impl Iterator<Item = (ExprId, Ty, Option<&Resolution>)> {
    inference
        .expr_types
        .iter()
        .map(|(id, ty)| (id, *ty, inference.expr_resolution(id)))
}

/// The `InferenceFailure` messages an owner produced.
fn failure_messages(inference: &Inference) -> Vec<String> {
    inference
        .diagnostics
        .iter()
        .filter_map(|diagnostic| match diagnostic {
            SemanticDiagnostic::InferenceFailure { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

/// `true` when the named function's body reports a type mismatch.
fn reports_type_mismatch(source: &str, function: &str) -> bool {
    let (db, file) = make_test_db(source);
    named_function_inference(&db, file, function)
        .diagnostics
        .iter()
        .any(|d| matches!(d, SemanticDiagnostic::TypeMismatch { .. }))
}

/// `(lhs, operator, type)` for every assignment expression in the owner's
/// body, in arena order.
fn assignments(db: &Db, def: DefId) -> Vec<(ExprId, Option<BinaryOp>, Ty)> {
    let DefId::Function(id) = def else {
        return Vec::new();
    };
    let body = db.body_map(BodyOwnerId::Function(id));
    let inference = db.inference(def);
    body.syntax_store
        .exprs
        .iter()
        .filter_map(|(expr, e)| match e {
            Expr::Assign { lhs, op, .. } => Some((*lhs, *op, inference.expr_ty(expr))),
            _ => None,
        })
        .collect()
}

#[test]
fn declaration_names_are_semantic_hover_targets() {
    let (db, file) = make_test_db(
        r#"
            contract C {
                error Bad(uint256 code);
                event Done(uint256 value);
                function f() external {}
            }
            "#,
    );

    // Errors and events lower their declared name into their item store, so
    // `semantic_at` finds it and hover has a resolution to show. This is the
    // path `Hover::hover_target` walks: range → `SemanticId::Expr` →
    // `expr_resolution`.
    for def in [find_error(&db, file, "Bad"), find_event(&db, file, "Done")] {
        let name = def_name(&db, def);
        let range = def_name_range(&db, file, def).expect("def should have a name range");
        let id = semantic_at(&db, def, range)
            .unwrap_or_else(|| panic!("`{name}` should map to a semantic id"));
        let SemanticId::Expr(expr) = id else {
            panic!("`{name}` should be an expression");
        };
        let resolved = db
            .inference(def)
            .expr_resolution(expr)
            .and_then(|resolution| match resolution {
                Resolution::Callable(callable) => callable.def(),
                _ => None,
            });
        assert_eq!(
            resolved,
            Some(def),
            "`{name}` should resolve to its own declaration"
        );
    }
}
/// An invocation wrapper (`emit`, `revert`, a modifier invocation) shares its
/// syntax node with the call it wraps, so it must not claim the range→semantic
/// mapping: hover and definition on the statement must still resolve to the
/// selected declaration, which the call carries.
#[test]
fn invocation_wrapper_does_not_steal_the_semantic_mapping() {
    let (db, file) = make_test_db(
        "contract C { event Done(uint256 v); function f() external { emit Done(1); } }",
    );
    let f = find_function(&db, file, "f");
    let DefId::Function(id) = f else {
        panic!("fixture declares function f")
    };
    let body = db.body_map(BodyOwnerId::Function(id));
    let (wrapper, _) = body
        .syntax_store
        .exprs
        .iter()
        .find(|(_, expr)| matches!(expr, Expr::Emit(_)))
        .expect("the fixture has an emit statement");
    let range = db.body_source_map(BodyOwnerId::Function(id)).expr_ranges[wrapper];

    let Some(SemanticId::Expr(mapped)) = semantic_at(&db, f, range) else {
        panic!("the emit statement's range should map to an expression");
    };
    assert_ne!(
        mapped, wrapper,
        "the wrapper must not own the statement range"
    );
    assert!(
        matches!(
            db.inference(f).expr_resolution(mapped),
            Some(Resolution::Callable(_))
        ),
        "the mapped expression should carry the selected event",
    );
}

#[test]
fn computed_expression_keeps_type_without_a_resolution() {
    let (db, file) =
        make_test_db("contract C { function f(uint a, uint b) external { uint c = a + b; } }");
    let inference = function_inference(&db, file);
    assert!(expr_results(&inference).any(|(_, ty, resolution)| {
        resolution.is_none() && !matches!(ty.kind(&db), TyKind::Invalid)
    }));
}
#[test]
fn every_expression_has_a_type_and_resolution_is_sparse() {
    let (db, file) =
        make_test_db("contract C { function f(uint a, uint b) external { uint c = a + b; } }");
    let inference = function_inference(&db, file);
    let DefId::Function(function) = find_function(&db, file, "f") else {
        panic!("fixture should contain a function")
    };
    let body = db.body_map(BodyOwnerId::Function(function));
    assert_eq!(
        inference.expr_types.iter().count(),
        body.syntax_store.exprs.iter().count()
    );
    assert!(inference.expr_resolutions.iter().count() < inference.expr_types.iter().count());
    for (expr, _) in body.syntax_store.exprs.iter() {
        let _ = inference.expr_ty(expr);
    }
}
#[test]
fn tables_are_keyed_by_expr_id() {
    let (db, file) = make_test_db("contract C { function f() external { uint x = 1 + 2; } }");
    let inference = function_inference(&db, file);
    // The binary expression has a type but no declaration identity.
    let binary = expr_results(&inference)
        .find(|(_, _, resolution)| resolution.is_none())
        .map(|(id, _, _)| id)
        .expect("expected a type-only binary result");
    assert!(inference.expr_types.get(binary).is_some());
}
#[test]
fn cycle_recovery_fills_all_allocated_types_with_invalid() {
    let (db, file) =
        make_test_db(r#"contract C { uint256 constant A = B; uint256 constant B = A; }"#);
    let vars = db
        .file_def_map(file)
        .defs
        .keys()
        .copied()
        .filter(|def| matches!(def, DefId::Var(_)))
        .collect::<Vec<_>>();
    assert_eq!(vars.len(), 2);
    for var in vars {
        let inference = db.inference(var);
        let DefId::Var(id) = var else { continue };
        let data = db.var_data(id);
        for (expr, _) in data.syntax_store.exprs.iter() {
            assert!(matches!(inference.expr_ty(expr).kind(&db), TyKind::Invalid));
        }
        for (ty_ref_id, _) in data.syntax_store.types.iter() {
            assert!(matches!(inference.ty(ty_ref_id).kind(&db), TyKind::Invalid));
        }
    }
}
