use la_arena::Idx;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use triomphe::Arc;

use super::db::Db;
use super::interned_db::{
    BodyOwner, Contract, DefWithBases, DefWithBasesId, Enum, Error, Event, Function, Import,
    Interface, Library, Modifier, Struct, Udvt, Using, Var,
};
use super::root_db::RootDb;
use crate::ast::{self, FunctionKind, NodeRange, ToAstNode};
use crate::hir::analysis::Analysis;
use crate::hir::body_map::{BodyBuilder, BodyMap, BodyOwnerId, BodySourceMap, ByteOffset, LocalId};
use crate::hir::member_map::{MemberBuilder, MemberMap};
use crate::hir::docs::{docs, inline_docs};
use crate::hir::exprs::Name;
use crate::hir::inference::{Inference, Inferer};
use crate::hir::syntax_store::{OwnerStore, with_owner_store};
use crate::hir::signatures::getter_signature;
use crate::hir::item_data::{
    ContractData, EnumData, ErrorData, EventData, FunctionData, ImportData,
    InterfaceData, ItemBuilder, LibraryData, ModifierData, StructData, UdvtData,
    UsingData, VarData,
};
use crate::hir::linearize::Linearizer;
use crate::hir::overrides::{OverrideAnalyzer, OverrideFacts};
use crate::hir::resolution::Resolution;
use crate::hir::semantic_errors::SemanticDiagnostic;
use crate::hir::types::{Fn, Ty, Type};
use crate::ir::def_map::{DefId, DefMap, DefNamespace, Scope};
use crate::salsa::File;
use crate::salsa::interned_db::Id;



#[salsa::db]
pub trait HirDb: RootDb {
    fn body_map(&self, owner: BodyOwnerId) -> Arc<BodyMap>;
    fn body_source_map(&self, owner: BodyOwnerId) -> Arc<BodySourceMap>;
    fn body_and_source_map(&self, owner: BodyOwnerId) -> (Arc<BodyMap>, Arc<BodySourceMap>);
    fn inference(&self, owner: DefId) -> Arc<Inference>;
    fn function_data(&self, id: ast::FunctionId) -> Arc<FunctionData>;
    fn struct_data(&self, id: ast::StructId) -> Arc<StructData>;
    fn enum_data(&self, id: ast::EnumId) -> Arc<EnumData>;
    fn var_data(&self, id: ast::VarId) -> Arc<VarData>;
    fn error_data(&self, id: ast::ErrorId) -> Arc<ErrorData>;
    fn modifier_data(&self, id: ast::ModifierId) -> Arc<ModifierData>;
    fn function_signature(&self, id: ast::FunctionId) -> Option<Fn<Type>>;
    fn getter_signature(&self, id: ast::VarId) -> Option<Fn<Type>>;
    fn error_signature(&self, id: ast::ErrorId) -> Option<Fn<Type>>;
    fn modifier_signature(&self, id: ast::ModifierId) -> Option<Fn<Type>>;
    fn event_signature(&self, id: ast::EventId) -> Option<Fn<Type>>;
    fn event_data(&self, id: ast::EventId) -> Arc<EventData>;
    fn contract_data(&self, id: ast::ContractId) -> Arc<ContractData>;
    fn interface_data(&self, id: ast::InterfaceId) -> Arc<InterfaceData>;
    fn library_data(&self, id: ast::LibraryId) -> Arc<LibraryData>;
    fn import_data(&self, id: ast::ImportId) -> Arc<ImportData>;
    fn udvt_data(&self, id: ast::UdvtId) -> Arc<UdvtData>;
    fn using_data(&self, id: ast::UsingId) -> Arc<UsingData>;
    fn bases(&self, def: DefId) -> Arc<[DefId]>;
    fn bases_analysis(&self, def: DefId) -> Analysis<[DefId]>;
    fn member_map(&self, def: DefId) -> Analysis<MemberMap>;
    fn owner(&self, def: DefId) -> Option<DefId>;
    fn docs(&self, def: DefId) -> Option<String>;
    fn inline_docs(&self, file: File, range: NodeRange) -> Option<String>;
    fn override_facts(&self, def: DefId) -> Analysis<OverrideFacts>;
    fn using_index(&self, def: DefId) -> UsingIndex;
}

#[salsa::db]
impl HirDb for Db {
    fn body_map(&self, owner: BodyOwnerId) -> Arc<BodyMap> {
        body_map(self, BodyOwner::new(self, owner)).0
    }

    fn body_source_map(&self, owner: BodyOwnerId) -> Arc<BodySourceMap> {
        body_map(self, BodyOwner::new(self, owner)).1
    }

    fn body_and_source_map(&self, owner: BodyOwnerId) -> (Arc<BodyMap>, Arc<BodySourceMap>) {
        body_map(self, BodyOwner::new(self, owner))
    }

    fn inference(&self, owner: DefId) -> Arc<Inference> {
        inference(self, Id::new(self, owner))
    }

    fn function_data(&self, id: ast::FunctionId) -> Arc<FunctionData> {
        function_data(self, Function::new(self, id))
    }

    fn function_signature(&self, id: ast::FunctionId) -> Option<Fn<Type>> {
        function_signature(self, Function::new(self, id))
    }

    fn getter_signature(&self, id: ast::VarId) -> Option<Fn<Type>> {
        getter_fn_ty(self, Var::new(self, id))
    }

    fn struct_data(&self, id: ast::StructId) -> Arc<StructData> {
        struct_data(self, Struct::new(self, id))
    }

    fn enum_data(&self, id: ast::EnumId) -> Arc<EnumData> {
        enum_data(self, Enum::new(self, id))
    }

    fn var_data(&self, id: ast::VarId) -> Arc<VarData> {
        var_data(self, Var::new(self, id))
    }

    fn error_data(&self, id: ast::ErrorId) -> Arc<ErrorData> {
        error_data(self, Error::new(self, id))
    }

    fn error_signature(&self, id: ast::ErrorId) -> Option<Fn<Type>> {
        error_signature(self, Error::new(self, id))
    }

    fn modifier_data(&self, id: ast::ModifierId) -> Arc<ModifierData> {
        modifier_data(self, Modifier::new(self, id))
    }

    fn modifier_signature(&self, id: ast::ModifierId) -> Option<Fn<Type>> {
        modifier_signature(self, Modifier::new(self, id))
    }

    fn event_data(&self, id: ast::EventId) -> Arc<EventData> {
        event_data(self, Event::new(self, id))
    }

    fn event_signature(&self, id: ast::EventId) -> Option<Fn<Type>> {
        event_signature(self, Event::new(self, id))
    }

    fn contract_data(&self, id: ast::ContractId) -> Arc<ContractData> {
        contract_data(self, Contract::new(self, id))
    }

    fn interface_data(&self, id: ast::InterfaceId) -> Arc<InterfaceData> {
        interface_data(self, Interface::new(self, id))
    }

    fn library_data(&self, id: ast::LibraryId) -> Arc<LibraryData> {
        library_data(self, Library::new(self, id))
    }

    fn import_data(&self, id: ast::ImportId) -> Arc<ImportData> {
        import_data(self, Import::new(self, id))
    }

    fn udvt_data(&self, id: ast::UdvtId) -> Arc<UdvtData> {
        udvt_data(self, Udvt::new(self, id))
    }

    fn using_data(&self, id: ast::UsingId) -> Arc<UsingData> {
        using_data(self, Using::new(self, id))
    }

    fn bases(&self, def: DefId) -> Arc<[DefId]> {
        self.bases_analysis(def).data.clone()
    }

    fn owner(&self, def: DefId) -> Option<DefId> {
        owner(self, Id::new(self, def))
    }

    fn bases_analysis(&self, def: DefId) -> Analysis<[DefId]> {
        let def = match def {
            DefId::Contract(c) => DefWithBases::new(self, DefWithBasesId::Contract(c)),
            DefId::Interface(i) => DefWithBases::new(self, DefWithBasesId::Interface(i)),
            _ => return Analysis::new(Vec::<DefId>::new(), Arc::from(Vec::new())),
        };

        bases(self, def)
    }

    fn member_map(&self, def: DefId) -> Analysis<MemberMap> {
        let def = match def {
            DefId::Contract(c) => DefWithBases::new(self, DefWithBasesId::Contract(c)),
            DefId::Interface(i) => DefWithBases::new(self, DefWithBasesId::Interface(i)),
            _ => {
                return Analysis::new(MemberMap::default(), Arc::from(Vec::new()));
            }
        };
        member_map(self, def)
    }

    fn docs(&self, def: DefId) -> Option<String> {
        docs(self, Id::new(self, def))
    }

    fn inline_docs(&self, file: File, range: NodeRange) -> Option<String> {
        inline_docs(self, file, range)
    }

    fn override_facts(&self,def: DefId) -> Analysis<OverrideFacts> {
        let def = match def {
            DefId::Contract(c) => DefWithBases::new(self, DefWithBasesId::Contract(c)),
            DefId::Interface(i) => DefWithBases::new(self, DefWithBasesId::Interface(i)),
            _ => return Analysis::new(OverrideFacts::default(), Arc::from(Vec::new())),
        };
        override_facts(self, def)
    }

    fn using_index(&self, def: DefId) -> UsingIndex {
        match def {
            DefId::File(f) => collect_file_using(self, f),
            DefId::Contract(_) | DefId::Library(_) | DefId::Interface(_) => {
                collect_container_using(self, Id::new(self, def))
            }
            _ => UsingIndex::default(),
        }
    }
}



/// A def's display name: the written name from its item data, or a
/// synthesized name for defs that have none — `import`, `using`, `file`,
/// `constructor`, `fallback`, `receive`. For the written name only, use
/// `root_db::def_name_and_range`.
pub fn def_name(db: &dyn HirDb, def: DefId) -> String {
    match def {
        DefId::Function(id) => {
            let data = db.function_data(id);
            if data.name.is_empty() {
                match data.kind {
                    FunctionKind::Constructor => "constructor".to_string(),
                    FunctionKind::Fallback => "fallback".to_string(),
                    FunctionKind::Receive => "receive".to_string(),
                    FunctionKind::Regular => data.name.to_string(),
                }
            } else {
                data.name.to_string()
            }
        }
        DefId::Struct(id) => db.struct_data(id).name.to_string(),
        DefId::Enum(id) => db.enum_data(id).name.to_string(),
        DefId::Var(id) => db.var_data(id).name.to_string(),
        DefId::Error(id) => db.error_data(id).name.to_string(),
        DefId::Modifier(id) => db.modifier_data(id).name.to_string(),
        DefId::Event(id) => db.event_data(id).name.to_string(),
        DefId::Contract(id) => db.contract_data(id).name.to_string(),
        DefId::Interface(id) => db.interface_data(id).name.to_string(),
        DefId::Library(id) => db.library_data(id).name.to_string(),
        DefId::Import(_) => "import".to_string(),
        DefId::Udvt(id) => db.udvt_data(id).name.to_string(),
        DefId::Using(_) => "using".to_string(),
        DefId::File(_) => "file".to_string(),
    }
}

#[salsa::tracked]
pub fn body_map<'db>(db: &'db dyn RootDb, owner: BodyOwner<'db>) -> (Arc<BodyMap>, Arc<BodySourceMap>) {
    tracing::debug!(?owner, "body_map: cache miss");

    let (owner, ast_root) = match owner.id(db) {
        BodyOwnerId::Function(id) => {
            let ast_root = db.ast(id.file).root();
            let ast_id_map = db.ast_id_map(id.file);
            (
                ast_id_map.get(&ast_root, id.id).unwrap().to_node(),
                ast_root,
            )
        }
        BodyOwnerId::Modifier(id) => {
            let ast_root = db.ast(id.file).root();
            let ast_id_map = db.ast_id_map(id.file);
            (
                ast_id_map.get(&ast_root, id.id).unwrap().to_node(),
                ast_root,
            )
        }
    };
    let (body, source_map) = BodyBuilder::build(ast_root, owner).unwrap();
    (body.into(), source_map.into())
}

/// Signature params — every parameter carries its default or written data
/// location, so no slot needs to re-derive one.
fn lower_params(
    inferer: &Inference,
    ids: &[LocalId],
    db: &dyn HirDb,
) -> Option<Box<[Type]>> {
    ids.iter()
        .map(|id| {
            let ty = inferer.local_ty(*id);
            (!ty.is_invalid(db)).then_some(ty)
        })
        .collect::<Option<Vec<_>>>()
        .map(Vec::into_boxed_slice)
}

#[salsa::tracked]
pub fn function_signature<'db>(db: &'db dyn HirDb, f: Function<'db>) -> Option<Fn<Type>> {
    let id = f.id(db);
    let data = function_data(db, f);
    tracing::debug!(name = %data.name, ?id, "function_signature: cache miss");
    let fn_inference = Inferer::run(
        db,
        DefId::Function(id),
        OwnerStore::new(&data.syntax_store).with_locals(&data.parameters),
    );

    Some(Fn {
        visibility: data.vis,
        mutability: data.mutability,
        params: lower_params(&fn_inference, &data.args, db)?,
        returns: lower_params(&fn_inference, &data.rets, db)?,
    })
}

/// The external function type a public state-variable getter denotes — the
/// pointer `this.x` / `c.x` produces. A getter is external by construction and
/// reads state, so `getter_signature` fixes its visibility and mutability.
#[salsa::tracked]
pub fn getter_fn_ty<'db>(db: &'db dyn HirDb, v: Var<'db>) -> Option<Fn<Type>> {
    let id = v.id(db);
    let data = var_data(db, v);
    let ty = db.inference(DefId::Var(id)).ty(data.ty_ref);
    (!ty.is_invalid(db)).then(|| getter_signature(db, ty))
}

#[salsa::tracked]
pub fn event_signature<'db>(db: &'db dyn HirDb, e: Event<'db>) -> Option<Fn<Type>> {
    let id = e.id(db);
    let data = event_data(db, e);
    let event_inference = db.inference(DefId::Event(id));
    let ids: Vec<LocalId> = data.parameters.iter().map(|(id, _)| id).collect();
    Some(Fn {
        params: lower_params(&event_inference, &ids, db)?,
        ..Fn::default()
    })
}

#[salsa::tracked]
pub fn error_signature<'db>(db: &'db dyn HirDb, e: Error<'db>) -> Option<Fn<Type>> {
    let id = e.id(db);
    let data = error_data(db, e);
    let error_inference = db.inference(DefId::Error(id));
    let ids: Vec<LocalId> = data.parameters.iter().map(|(id, _)| id).collect();
    Some(Fn {
        params: lower_params(&error_inference, &ids, db)?,
        ..Fn::default()
    })
}

#[salsa::tracked]
pub fn modifier_signature<'db>(db: &'db dyn HirDb, m: Modifier<'db>) -> Option<Fn<Type>> {
    let id = m.id(db);
    let data = modifier_data(db, m);
    let mod_inference = Inferer::run(
        db,
        DefId::Modifier(id),
        OwnerStore::new(&data.syntax_store).with_locals(&data.parameters),
    );
    let ids: Vec<LocalId> = data.parameters.iter().map(|(id, _)| id).collect();
    Some(Fn {
        params: lower_params(&mod_inference, &ids, db)?,
        ..Fn::default()
    })
}

#[salsa::tracked]
pub fn function_data<'db>(db: &'db dyn RootDb, f: Function<'db>) -> Arc<FunctionData> {
    tracing::debug!(?f, "function_data: cache miss");
    let id = f.id(db);
    let ast_id_map = db.ast_id_map(id.file);
    let root = db.root(id.file);
    let func = ast_id_map.get(&root, id.id).unwrap();
    Arc::new(ItemBuilder::new(root, id.file, ast_id_map).build_fn(&func))
}

#[salsa::tracked]
pub fn struct_data<'db>(db: &'db dyn RootDb, s: Struct<'db>) -> Arc<StructData> {
    tracing::debug!(?s, "struct_data: cache miss");
    let id = s.id(db);
    let ast_id_map = db.ast_id_map(id.file);
    let root = db.root(id.file);
    let strukt = ast_id_map.get(&root, id.id).unwrap();
    Arc::new(ItemBuilder::new(root, id.file, ast_id_map).build_struct(&strukt))
}

#[salsa::tracked]
pub fn var_data<'db>(db: &'db dyn RootDb, v: Var<'db>) -> Arc<VarData> {
    tracing::debug!(?v, "var_data: cache miss");
    let id = v.id(db);
    let ast_id_map = db.ast_id_map(id.file);
    let root = db.root(id.file);
    let var = ast_id_map.get(&root, id.id).unwrap();
    Arc::new(ItemBuilder::new(root, id.file, ast_id_map).build_var(&var))
}

#[salsa::tracked]
pub fn enum_data<'db>(db: &'db dyn RootDb, e: Enum<'db>) -> Arc<EnumData> {
    tracing::debug!(?e, "enum_data: cache miss");
    let id = e.id(db);
    let ast_id_map = db.ast_id_map(id.file);
    let root = db.root(id.file);
    let enom = ast_id_map.get(&root, id.id).unwrap();
    Arc::new(ItemBuilder::new(root, id.file, ast_id_map).build_enum(&enom))
}

#[salsa::tracked]
pub fn event_data<'db>(db: &'db dyn RootDb, e: Event<'db>) -> Arc<EventData> {
    tracing::debug!(?e, "event_data: cache miss");
    let id = e.id(db);
    let ast_id_map = db.ast_id_map(id.file);
    let root = db.root(id.file);
    let event = ast_id_map.get(&root, id.id).unwrap();
    Arc::new(ItemBuilder::new(root, id.file, ast_id_map).build_event(&event))
}

#[salsa::tracked]
pub fn error_data<'db>(db: &'db dyn RootDb, e: Error<'db>) -> Arc<ErrorData> {
    tracing::debug!(?e, "error_data: cache miss");
    let id = e.id(db);
    let ast_id_map = db.ast_id_map(id.file);
    let root = db.root(id.file);
    let error = ast_id_map.get(&root, id.id).unwrap();
    Arc::new(ItemBuilder::new(root, id.file, ast_id_map).build_error(&error))
}

#[salsa::tracked]
pub fn modifier_data<'db>(db: &'db dyn RootDb, m: Modifier<'db>) -> Arc<ModifierData> {
    tracing::debug!(?m, "modifier_data: cache miss");
    let id = m.id(db);
    let ast_id_map = db.ast_id_map(id.file);
    let root = db.root(id.file);
    let modifier = ast_id_map.get(&root, id.id).unwrap();
    Arc::new(ItemBuilder::new(root, id.file, ast_id_map).build_modifier(&modifier))
}

#[salsa::tracked]
pub fn contract_data<'db>(db: &'db dyn RootDb, c: Contract<'db>) -> Arc<ContractData> {
    tracing::debug!(?c, "contract_data: cache miss");
    let id = c.id(db);
    let ast_id_map = db.ast_id_map(id.file);
    let root = db.root(id.file);
    let contract = ast_id_map.get(&root, id.id).unwrap();
    Arc::new(ItemBuilder::new(root, id.file, ast_id_map).build_contract(&contract))
}

#[salsa::tracked]
pub fn interface_data<'db>(db: &'db dyn RootDb, i: Interface<'db>) -> Arc<InterfaceData> {
    tracing::debug!(?i, "interface_data: cache miss");
    let id = i.id(db);
    let ast_id_map = db.ast_id_map(id.file);
    let root = db.root(id.file);
    let interface = ast_id_map.get(&root, id.id).unwrap();
    Arc::new(ItemBuilder::new(root, id.file, ast_id_map).build_interface(&interface))
}

#[salsa::tracked]
pub fn library_data<'db>(db: &'db dyn RootDb, l: Library<'db>) -> Arc<LibraryData> {
    tracing::debug!(?l, "library_data: cache miss");
    let id = l.id(db);
    let ast_id_map = db.ast_id_map(id.file);
    let root = db.root(id.file);
    let library = ast_id_map.get(&root, id.id).unwrap();
    Arc::new(ItemBuilder::new(root, id.file, ast_id_map).build_library(&library))
}

#[salsa::tracked]
pub fn import_data<'db>(db: &'db dyn RootDb, i: Import<'db>) -> Arc<ImportData> {
    tracing::debug!(?i, "import_data: cache miss");
    let id = i.id(db);
    let ast_id_map = db.ast_id_map(id.file);
    let root = db.root(id.file);
    let import = ast_id_map.get(&root, id.id).unwrap();
    Arc::new(ItemBuilder::new(root, id.file, ast_id_map).build_import(&import))
}

#[salsa::tracked]
pub fn udvt_data<'db>(db: &'db dyn RootDb, u: Udvt<'db>) -> Arc<UdvtData> {
    tracing::debug!(?u, "udvt_data: cache miss");
    let id = u.id(db);
    let ast_id_map = db.ast_id_map(id.file);
    let root = db.root(id.file);
    let udvt = ast_id_map.get(&root, id.id).unwrap();
    Arc::new(ItemBuilder::new(root, id.file, ast_id_map).build_udvt(&udvt))
}

#[salsa::tracked]
pub fn using_data<'db>(db: &'db dyn RootDb, u: Using<'db>) -> Arc<UsingData> {
    tracing::debug!(?u, "using_data: cache miss");
    let id = u.id(db);
    let ast_id_map = db.ast_id_map(id.file);
    let root = db.root(id.file);
    let using = ast_id_map.get(&root, id.id).unwrap();
    Arc::new(ItemBuilder::new(root, id.file, ast_id_map).build_using(&using))
}

/// Salsa-tracked C3 linearization for a contract or interface.
///
/// Returns the linearized MRO including `def` itself as the first element,
/// together with inheritance diagnostics (unresolved bases, cyclic or
/// inconsistent hierarchies).
#[salsa::tracked(cycle_result = bases_cycle_error)]
pub fn bases<'db>(db: &'db dyn HirDb, def: DefWithBases<'db>) -> Analysis<[DefId]> {
    tracing::debug!(?def, "bases: cache miss");
    let id = def.id(db);
    let container = match id {
        DefWithBasesId::Contract(c) => DefId::Contract(c),
        DefWithBasesId::Interface(i) => DefId::Interface(i),
    };
    Linearizer::new(db, container).c3_linearize()
}

fn bases_cycle_error<'db>(
    db: &'db dyn HirDb,
    _id: salsa::Id,
    def: DefWithBases<'db>,
) -> Analysis<[DefId]> {
    let container = match def.id(db) {
        DefWithBasesId::Contract(c) => DefId::Contract(c),
        DefWithBasesId::Interface(i) => DefId::Interface(i),
    };
    Analysis::new(
        vec![container],
        Arc::from(vec![SemanticDiagnostic::InheritanceFailure {
            contract: container,
            problems: vec!["cyclic inheritance hierarchy".to_string()],
        }]),
    )
}

/// Cached declaration-to-base facts used recursively by the single public
/// `member_map` query. It does not compute effective slots or diagnostics
/// for inherited conflicts.
#[salsa::tracked(cycle_result = override_facts_cycle_result)]
fn override_facts<'db>(
    db: &'db dyn HirDb,
    def: DefWithBases<'db>,
) -> Analysis<OverrideFacts> {
    let container = match def.id(db) {
        DefWithBasesId::Contract(c) => DefId::Contract(c),
        DefWithBasesId::Interface(i) => DefId::Interface(i),
    };
    OverrideAnalyzer::new(db, container).analyze()
}

fn override_facts_cycle_result<'db>(
    _db: &'db dyn HirDb,
    _id: salsa::Id,
    _def: DefWithBases<'db>,
) -> Analysis<OverrideFacts> {
    Analysis::new(
        OverrideFacts {
            complete: false,
            ..Default::default()
        },
        Arc::from(Vec::new()),
    )
}

#[salsa::tracked(cycle_result = member_map_cycle_result)]
pub fn member_map<'db>(
    db: &'db dyn HirDb,
    def: DefWithBases<'db>,
) -> Analysis<MemberMap> {
    let container = match def.id(db) {
        DefWithBasesId::Contract(c) => DefId::Contract(c),
        DefWithBasesId::Interface(i) => DefId::Interface(i),
    };
    MemberBuilder::new(db, container).build()
}

fn member_map_cycle_result<'db>(
    _db: &'db dyn HirDb,
    _id: salsa::Id,
    _def: DefWithBases<'db>,
) -> Analysis<MemberMap> {
    Analysis::new(
        MemberMap {
            complete: false,
            ..MemberMap::default()
        },
        Arc::from(Vec::new()),
    )
}


/// Cycle fallback: initializers and struct fields can reference each other
/// cyclically, so fill every allocated ID with `Invalid` to keep tables total.
#[salsa::tracked(cycle_result = inference_cycle_result)]
pub fn inference<'db>(db: &'db dyn HirDb, owner: Id<'db>) -> Arc<Inference> {
    let owner = owner.id(db);
    let name = def_name(db, owner);
    tracing::debug!(%name, ?owner, "inference: cache miss");

    match with_owner_store(db, owner, |input| Inferer::run(db, owner, input)) {
        Some(inference) => inference.into(),
        None => Inference::empty().into(),
    }
}

fn inference_cycle_result<'db>(
    db: &'db dyn HirDb,
    _id: salsa::Id,
    owner: Id<'db>,
) -> Arc<Inference> {
    let tables = match with_owner_store(db, owner.id(db), |input| {
        Inference::invalid_for(db, input.store, input.locals, input.fields, input.variants)
    }) {
        Some(tables) => tables,
        None => Inference::empty(),
    };
    tables.into()
}

/// The declaration containing `offset` — the innermost enclosing container in
/// `file`'s def map, or `DefId::File` when the position is outside every
/// declaration.
pub fn owner_at(db: &Db, file: File, offset: ByteOffset) -> Option<DefId> {
    let node = db.named_node_at(file, offset)?;
    db.enclosing_containers(node.node(), file).last().copied()
}


#[derive(Default, Clone, PartialEq, Eq)]
pub struct UsingIndex {
    pub exact: FxHashMap<Ty, FxHashMap<Name, SmallVec<[DefId; 1]>>>,
    pub any: FxHashMap<Name, SmallVec<[DefId; 1]>>,
}

impl UsingIndex {
    pub fn merge(&mut self, other: UsingIndex) {
        for (ty, by_name) in other.exact {
            let entry = self.exact.entry(ty).or_default();
            for (name, defs) in by_name {
                entry.entry(name).or_default().extend(defs);
            }
        }
        for (name, defs) in other.any {
            self.any.entry(name).or_default().extend(defs);
        }
    }
}

#[salsa::tracked]
pub fn owner<'db>(db: &'db dyn HirDb, id: Id<'db>) -> Option<DefId> {
    let def = id.id(db);
    let defmap = db.file_def_map(def.file());
    let data = defmap.defs.get(&def)?;
    Some(defmap.scopes[data.scope].owner)
}

#[salsa::tracked]
pub fn resolve_using<'db>(db: &'db dyn HirDb, using: Using<'db>) -> UsingIndex {
    tracing::debug!(?using, "resolve_using: cache miss");
    let mut using_index = UsingIndex::default();
    let using_data = using_data(db, using);
    let id = using.id(db);
    let def_id = DefId::Using(id);
    let inference = db.inference(def_id);
    // Keys are location-free: receivers are peeled before the lookup.
    let target = using_data
        .target
        .map(|t| inference.ty(t).ty().peel_loc(db));

    let mut collect = |fn_def: DefId| {
        let DefId::Function(f_id) = fn_def else {
            return;
        };
        let fn_data = function_data(db, Function::new(db, f_id));
        if fn_data.args.is_empty() {
            //can't attach fn with no params
            return;
        }
        match &target {
            Some(target_ty) => {
                using_index
                    .exact
                    .entry(*target_ty)
                    .or_default()
                    .entry(fn_data.name.clone())
                    .or_default()
                    .push(fn_def);
            }
            None => {
                using_index
                    .any
                    .entry(fn_data.name.clone())
                    .or_default()
                    .push(fn_def);
            }
        }
    };

    for source in &using_data.sources {
        let Some(res) = inference.expr_resolution(*source).cloned() else {
            continue;
        };

        match res {
            // using sources can only be fns and libs, fns are resolved as defs and libs as types
            Resolution::Callable(callable) => {
                if let Some(def @ DefId::Function(_)) = callable.def() {
                    collect(def);
                }
            }
            Resolution::Callables(callables) => {
                for callable in callables.iter() {
                    if let Some(def @ DefId::Function(_)) = callable.def() {
                        collect(def);
                    }
                }
            }
            Resolution::Type(ty) => {
                if let Some(def @ DefId::Library(_)) = ty.def_id(db) {
                    let defmap = db.file_def_map(def.file());
                    let data = defmap.defs.get(&def).unwrap();
                    let scope = &defmap.scopes[data.child_scope.unwrap()];

                    let fns: Vec<DefId> = scope
                        .by_name
                        .values()
                        .filter(|sd| sd.namespace == DefNamespace::Functions)
                        .flat_map(|sd| sd.defs.iter().copied())
                        .collect();

                    for fn_def in fns {
                        collect(fn_def);
                    }
                }
            }
            _ => {}
        };
    }

    using_index
}

#[salsa::tracked]
pub fn collect_file_using(db: &dyn HirDb, file: File) -> UsingIndex {
    let mut index = UsingIndex::default();
    let defmap = db.file_def_map(file);
    if let Some(file_data) = defmap.files.get(&file) {
        collect_scope_usings(db, &defmap, file_data.scope, &mut index);
    }
    index
}

fn collect_scope_usings(
    db: &dyn HirDb,
    defmap: &DefMap,
    scope_id: Idx<Scope>,
    index: &mut UsingIndex,
) {
    let scope = &defmap.scopes[scope_id];
    if let Some(usings) = &scope.usings {
        for using_def in usings {
            if let DefId::Using(using_id) = using_def {
                let using = Using::new(db, *using_id);
                let resolved = resolve_using(db, using);
                index.merge(resolved);
            }
        }
    }
}

#[salsa::tracked]
fn collect_container_using<'db>(db: &'db dyn HirDb, id: Id<'db>) -> UsingIndex {
    let def_id = id.id(db);
    let (file, _) = def_id.file_and_id();
    let mut index = collect_file_using(db, file);

    let defmap = db.root_def_map(db.file_source_root(file));
    if let Some(data) = defmap.defs.get(&def_id)
        && let Some(child_scope) = data.child_scope
    {
        collect_scope_usings(db, &defmap, child_scope, &mut index);
    }
    index
}

