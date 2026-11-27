use rustc_hash::FxHashSet;

use crate::ast::NodeRange;
use crate::capabilities::{Identity, identity_of_semantic};
use crate::hir::body_map::BodyOwnerId;
use crate::hir::item_data::SemanticId;
use crate::hir::resolution::{Field, Resolution};
use crate::hir::syntax_store::{for_each_semantic, semantic_at};
use crate::hir::tests::helper::{def_name_range, find_contract, find_function, make_test_db};
use crate::ir::def_map::DefId;
use crate::salsa::HirDb;
use crate::salsa::root_db::RootDb;

/// One fixture holding every declaration kind that carries a name.
const EVERY_KIND: &str = r#"
    type Price is uint256;

    error Bad(uint256 code);
    event Done(uint256 value);
    struct S { uint256 a; }
    enum E { A, B }
    uint256 constant TOP = 1;
    function freeF(uint256 x) pure returns (uint256) { return x; }

    contract C {
        uint256 state;
        struct Inner { uint256 b; }
        enum InnerE { X }
        event InnerEvent(uint256 v);
        error InnerError(uint256 c);
        modifier m() { _; }
        function f() public m {}
        constructor() {}
        fallback() external {}
        receive() external payable {}
        type InnerT is uint256;
    }

    interface I { function i() external; }
    library L { function l() internal pure {} }
"#;

/// Every named declaration lowers its own name into its store, and that
/// occurrence denotes the declaration — the invariant `rename` needs to treat a
/// declaration site as a reference.
#[test]
fn every_declaration_name_maps_to_its_own_identity() {
    let (db, file) = make_test_db(EVERY_KIND);
    let defmap = db.file_def_map(file);

    let mut checked = FxHashSet::default();
    for &def in defmap.defs.keys() {
        if def.file() != file {
            continue;
        }
        // Constructors, `fallback`, `receive` and `using` carry no name.
        let Some(range) = def_name_range(&db, file, def) else {
            continue;
        };
        let id = semantic_at(&db, def, range).unwrap_or_else(|| panic!("`{def:?}` lowers no name at its declaration"));
        let identity = identity_of_semantic(&db, def, id);
        assert!(identity == Some(Identity::Def(def)), "`{def:?}` resolves to {identity:?}");
        checked.insert(kind(def));
    }

    for expected in [
        "contract",
        "interface",
        "library",
        "struct",
        "enum",
        "udvt",
        "function",
        "modifier",
        "var",
        "event",
        "error",
    ] {
        assert!(checked.contains(expected), "`{expected}` is not covered: {checked:?}");
    }
}

/// The declaration kind, for the coverage assertion above.
fn kind(def: DefId) -> &'static str {
    match def {
        DefId::File(_) => "file",
        DefId::Import(_) => "import",
        DefId::Udvt(_) => "udvt",
        DefId::Using(_) => "using",
        DefId::Contract(_) => "contract",
        DefId::Interface(_) => "interface",
        DefId::Library(_) => "library",
        DefId::Function(_) => "function",
        DefId::Modifier(_) => "modifier",
        DefId::Struct(_) => "struct",
        DefId::Event(_) => "event",
        DefId::Enum(_) => "enum",
        DefId::Error(_) => "error",
        DefId::Var(_) => "var",
    }
}

#[test]
fn a_local_use_site_and_its_declaration_share_an_identity() {
    let (db, file) = make_test_db("contract C { function f() external { uint x = 1; x = 2; } }");
    let f = find_function(&db, file, "f");
    let DefId::Function(id) = f else { unreachable!() };
    let map = db.body_source_map(BodyOwnerId::Function(id));
    let inference = db.inference(f);

    let local = map
        .range_to_semantic
        .values()
        .find_map(|id| match id {
            SemanticId::Local(local) => Some(*local),
            _ => None,
        })
        .expect("the fixture declares a local");

    let (decl_range, use_range) = map
        .range_to_semantic
        .iter()
        .fold((None, None), |(decl, used), (range, id)| match id {
            SemanticId::Local(declared) if *declared == local => (Some(*range), used),
            SemanticId::Expr(expr) => match inference.expr_resolution(*expr) {
                Some(Resolution::Local(resolved)) if resolved.id == local => (decl, Some(*range)),
                _ => (decl, used),
            },
            _ => (decl, used),
        });

    assert_ne!(decl_range, use_range, "the declaration and the use are distinct ranges");
    assert_eq!(
        identity_of_semantic(&db, f, SemanticId::Local(local)),
        identity_of_semantic(
            &db,
            f,
            semantic_at(&db, f, use_range.expect("the use site is lowered")).expect("the use site has a semantic id")
        ),
    );
}

#[test]
fn a_member_use_site_and_its_field_share_an_identity() {
    let (db, file) = make_test_db("struct S { uint256 a; } contract C { S s; function f() external { s.a = 1; } }");
    let defmap = db.file_def_map(file);
    let structure = defmap
        .defs
        .keys()
        .copied()
        .find(|def| matches!(def, DefId::Struct(_)))
        .expect("the fixture declares a struct");
    let field = db
        .source_map(structure)
        .field_ranges
        .iter()
        .map(|(field, _)| field)
        .next()
        .expect("one field");
    let declaration = identity_of_semantic(&db, structure, SemanticId::Field(field));

    let f = find_function(&db, file, "f");
    let inference = db.inference(f);
    let DefId::Function(function) = f else { unreachable!() };
    let map = db.body_source_map(BodyOwnerId::Function(function));
    let (range, id) = map
        .range_to_semantic
        .iter()
        .find(|(_, id)| match id {
            SemanticId::Expr(expr) => matches!(
                inference.expr_resolution(*expr),
                Some(Resolution::Field(Field::Struct(_, used))) if *used == field
            ),
            _ => false,
        })
        .expect("`s.a` is a field use");

    assert_eq!(
        declaration,
        identity_of_semantic(&db, f, *id),
        "`s.a` at {range:?} denotes the declared field"
    );
}

/// A callable's header is part of its store, so an `override` path is a
/// spelling the function owns — the seam hover, goto and rename all read.
#[test]
fn an_override_path_denotes_the_contract_it_names() {
    let source = r#"
        contract A { function g() public virtual {} }
        contract C is A { function g() public override(A) {} }
    "#;
    let (db, file) = make_test_db(source);
    let a = find_contract(&db, file, "A");
    let c = find_contract(&db, file, "C");
    let g = db
        .file_def_map(file)
        .defs
        .keys()
        .copied()
        .find(|def| matches!(def, DefId::Function(_)) && db.container(*def) == Some(c))
        .expect("`C` declares `g`");

    let text = db.text(file);
    let start = text.find("override(A)").expect("the fixture writes an override list") + "override(".len();
    let range = NodeRange {
        start: start as u32,
        end: start as u32 + 1,
    };

    let id = semantic_at(&db, g, range).expect("the override path is a spelling of `g`'s store");
    assert_eq!(identity_of_semantic(&db, g, id), Some(Identity::Def(a)));
}

/// An unnamed parameter's node range is its type's range, so the parameter must
/// not claim it — the type would stop being a spelling.
#[test]
fn an_unnamed_parameter_keeps_its_type() {
    let source = "struct S { uint256 a; } contract C { function f(S) external {} }";
    let (db, file) = make_test_db(source);
    let f = find_function(&db, file, "f");
    let s = db
        .file_def_map(file)
        .defs
        .keys()
        .copied()
        .find(|def| matches!(def, DefId::Struct(_)))
        .expect("the fixture declares a struct");

    let start = source.find("function f(S)").expect("the fixture writes the parameter") + "function f(".len();
    let range = NodeRange {
        start: start as u32,
        end: start as u32 + 1,
    };

    let id = semantic_at(&db, f, range).expect("the parameter's type is a spelling of `f`");
    assert_eq!(identity_of_semantic(&db, f, id), Some(Identity::Def(s)));
}

#[test]
fn a_contract_is_one_identity_from_every_spelling() {
    let (db, file) = make_test_db("contract B {} contract E { function f() external { B b = new B(); } }");
    let b = find_contract(&db, file, "B");
    let f = find_function(&db, file, "f");
    let DefId::Function(function) = f else { unreachable!() };
    let map = db.body_source_map(BodyOwnerId::Function(function));

    let mut spellings = 0;
    for (_, id) in map.range_to_semantic.iter() {
        if identity_of_semantic(&db, f, *id) == Some(Identity::Def(b)) {
            spellings += 1;
        }
    }
    assert!(
        spellings >= 2,
        "the type name and the `new B()` callee both denote `B`, saw {spellings}"
    );
}

#[test]
fn a_collided_declaration_still_denotes_itself() {
    // The def map withholds the second `S`, so its name must resolve to itself.
    let (db, file) = make_test_db("contract C { struct S { uint256 a; } struct S { uint256 b; } }");
    let defmap = db.file_def_map(file);
    let structures = defmap
        .defs
        .keys()
        .copied()
        .filter(|def| matches!(def, DefId::Struct(_)))
        .collect::<Vec<_>>();
    assert_eq!(structures.len(), 2, "both declarations keep a def");

    for structure in structures {
        let range = def_name_range(&db, file, structure).expect("a struct has a name");
        let id = semantic_at(&db, structure, range).expect("the name is lowered");
        assert_eq!(
            identity_of_semantic(&db, structure, id),
            Some(Identity::Def(structure)),
            "`{structure:?}` denotes itself"
        );
    }
}

#[test]
fn a_builtin_or_a_primitive_names_nothing() {
    let (db, file) = make_test_db("contract C { function f() external { uint x = 1; msg.sender; } }");
    let f = find_function(&db, file, "f");
    let DefId::Function(function) = f else { unreachable!() };
    let map = db.body_source_map(BodyOwnerId::Function(function));
    let inference = db.inference(f);

    let mut builtin_field = false;
    let mut primitive = false;
    for id in map.range_to_semantic.values() {
        match id {
            SemanticId::Expr(expr) if matches!(inference.expr_resolution(*expr), Some(Resolution::Field(Field::Builtin(_)))) => {
                builtin_field = true;
                assert!(identity_of_semantic(&db, f, *id).is_none(), "`msg.sender` names no declaration");
            }
            SemanticId::Type(ty) if inference.ty(*ty).ty().def_id(&db).is_none() => {
                primitive = true;
                assert!(identity_of_semantic(&db, f, *id).is_none(), "`uint` names no declaration");
            }
            _ => {}
        }
    }
    assert!(builtin_field, "the fixture reads `msg.sender`");
    assert!(primitive, "the fixture declares a `uint`");
}

/// The iteration surface the scan walks: every `(range, semantic)` pair the
/// def's store holds, and nothing for a file, which owns no store.
#[test]
fn for_each_semantic_visits_every_spelled_occurrence() {
    let (db, file) = make_test_db("contract C { function f() external { uint x = 1; x = 2; } }");
    let f = find_function(&db, file, "f");
    let DefId::Function(id) = f else { unreachable!() };
    let map = db.body_source_map(BodyOwnerId::Function(id));

    let mut visited = Vec::new();
    for_each_semantic(&db, f, |range, id| visited.push((range, id)));

    assert_eq!(visited.len(), map.range_to_semantic.len());
    for (range, id) in &visited {
        assert!(map.range_to_semantic.get(range) == Some(id));
    }
    assert!(
        visited.iter().any(|(_, id)| matches!(id, SemanticId::Local(_))),
        "the declaration is visited"
    );
    assert!(
        visited.iter().any(|(_, id)| matches!(id, SemanticId::Expr(_))),
        "the use is visited"
    );

    for_each_semantic(&db, DefId::File(file), |_, _| panic!("a file owns no store"));
}
