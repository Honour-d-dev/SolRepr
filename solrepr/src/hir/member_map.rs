use std::collections::{BTreeMap, hash_map::Entry};

use rustc_hash::{FxHashMap, FxHashSet};
use triomphe::Arc;

use crate::ast::FunctionKind;
use crate::hir::analysis::Analysis;
use crate::hir::builtins::MemberNamespace;
use crate::hir::exprs::Name;
use crate::hir::overrides::OverrideFacts;
use crate::hir::diagnostics::{DiagnosticSink, SemanticDiagnostic};
use crate::hir::signatures::OverrideSignature;
use crate::hir::types::{Mutability, Type, Visibility};
use crate::ir::def_map::{DefId, DefNamespace};
use crate::salsa::HirDb;
use crate::salsa::hir_db::def_name;
use crate::salsa::root_db::def_range;

#[derive(PartialEq, Eq, Hash)]
enum SlotKey {
    Overload(Name, DefNamespace, Box<[Type]>),
    NonOverloadable(Name),
}

#[derive(Clone, PartialEq, Eq)]
pub struct MemberDeclaration {
    pub def: DefId,
    pub owner: DefId,
    pub namespace: DefNamespace,
}

#[derive(Clone, PartialEq, Eq)]
pub struct MemberSlot {
    pub candidates: Box<[DefId]>,
    /// The slot's winner: 
    /// `Some(def)` when one declaration stands for the slot 
    /// `None` when the slot is a true conflict.
    pub effective: Option<DefId>,
}

impl MemberSlot {
    pub fn effective(&self) -> Option<DefId> {
        self.effective
    }
}

#[derive(Clone, PartialEq, Eq, Default)]
pub struct MemberGroup {
    pub types: Vec<MemberSlot>,
    pub values: Vec<MemberSlot>,
}

impl MemberGroup {
    pub fn get(&self, namespace: MemberNamespace) -> &[MemberSlot] {
        match namespace {
            MemberNamespace::Types => &self.types,
            MemberNamespace::Values => &self.values,
        }
    }

    fn get_mut(&mut self, namespace: MemberNamespace) -> &mut Vec<MemberSlot> {
        match namespace {
            MemberNamespace::Types => &mut self.types,
            MemberNamespace::Values => &mut self.values,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Default)]
pub struct MemberMap {
    pub by_name: BTreeMap<Name, MemberGroup>,
    pub declarations: FxHashMap<DefId, MemberDeclaration>,
    pub obligations: Box<[DefId]>,
    pub complete: bool,
}

impl MemberMap {
    pub fn get(&self, name: &str, namespace: MemberNamespace) -> &[MemberSlot] {
        self.by_name
            .get(name)
            .map_or(&[], |group| group.get(namespace))
    }

    pub fn declaration(&self, def: DefId) -> Option<&MemberDeclaration> {
        self.declarations.get(&def)
    }

    pub fn is_payable(&self, db: &dyn HirDb) -> bool {
        self.by_name
            .values()
            .flat_map(|group| &group.values)
            .filter_map(MemberSlot::effective)
            .any(|def| {
                let DefId::Function(id) = def else {
                    return false;
                };
                let data = db.function_data(id);
                data.kind == FunctionKind::Receive
                    || (data.kind == FunctionKind::Fallback && data.mutability == Mutability::Payable)
            })
    }

    pub fn has_unimplemented_members(&self) -> bool {
        !self.obligations.is_empty()
    }
}

pub struct MemberBuilder<'db> {
    db: &'db dyn HirDb,
    container: DefId,
    mro: Arc<[DefId]>,
    complete: bool,
    facts: Arc<OverrideFacts>,
    by_name: BTreeMap<Name, MemberGroup>,
    declarations: FxHashMap<DefId, MemberDeclaration>,
    obligations: Vec<DefId>,
    sink: DiagnosticSink,
}

impl<'db> MemberBuilder<'db> {
    pub fn new(db: &'db dyn HirDb, container: DefId) -> Self {
        let mut sink = DiagnosticSink::default();
        let override_facts = db.override_facts(container);
        sink.extend(override_facts.diagnostics.iter());
        let bases_analysis = db.bases_analysis(container);
        sink.extend(bases_analysis.diagnostics.iter());
        let complete = override_facts.data.complete
            && bases_analysis.diagnostics.is_empty()
            && !bases_analysis.data.is_empty();
        Self {
            db,
            container,
            mro: bases_analysis.data,
            complete,
            facts: override_facts.data,
            by_name: BTreeMap::new(),
            declarations: FxHashMap::default(),
            obligations: Vec::new(),
            sink,
        }
    }

    pub fn build(mut self) -> Analysis<MemberMap> {
        self.add_override_slots();
        self.add_unavailable_overrides();
        self.add_non_override_members();
        Analysis::new(
            MemberMap {
                by_name: self.by_name,
                declarations: self.declarations,
                obligations: self.obligations.into_boxed_slice(),
                complete: self.complete,
            },
            self.sink.finish(),
        )
    }

    fn add_override_slots(&mut self) {
        let Some(container) = self.mro.first().copied() else {
            return;
        };
        let facts = &self.facts;
        let mut signatures: Box<[_]> = facts.by_sig.keys().collect();
        signatures.sort_by_key(|signature| signature.sort_key(self.db));
        let is_concrete =
            matches!(container, DefId::Contract(id) if !self.db.contract_data(id).is_abstract);

        for &signature in &signatures {
            let overrides = &facts.by_sig[signature];
            let mut declarations = facts.declarations(signature);
            declarations.sort_by_key(|def| {
                let owner = facts.known.get(def).map(|entry| entry.owner);
                (
                    owner
                        .and_then(|owner| self.mro.iter().position(|&base| base == owner))
                        .unwrap_or(usize::MAX),
                    def_range(self.db, *def).map_or(0, |range| range.start),
                )
            });
            if declarations.is_empty() {
                continue;
            }
            for &def in &declarations {
                if let Some(entry) = facts.known.get(&def) {
                    self.declarations.entry(def).or_insert(MemberDeclaration {
                        def,
                        owner: entry.owner,
                        namespace: DefNamespace::from(&def),
                    });
                }
            }

            let mut effective = None;
            if let Some(&decl) = overrides.own.first() {
                // Duplicate own declarations are reported by
                // `DuplicateMemberSlot`; the slot itself stays conflicted.
                effective = (overrides.own.len() == 1).then_some(decl);
                if self.complete && facts.owes_implementation(decl) {
                    self.obligations.push(decl);
                }
            } else if !overrides.inherited.is_empty() {
                let (unresolved, unimplemented_cuts) = facts.resolve_slot(signature);
                let conflicting: Box<[_]> = unresolved
                    .iter()
                    .copied()
                    .filter(|entry| !unimplemented_cuts.contains(&entry.decl))
                    .collect();
                let owners: FxHashSet<DefId> =
                    conflicting.iter().map(|entry| entry.owner).collect();
                if conflicting.len() > 1
                    || (conflicting.is_empty() && unresolved.len() > 1)
                {
                    if self.complete && owners.len() > 1 {
                        self.sink.push(SemanticDiagnostic::OverrideConflict {
                            contract: container,
                            name: signature.name().clone(),
                            declarations: conflicting.iter().map(|entry| entry.decl).collect(),
                        });
                    }
                } else if let Some(&entry) =
                    conflicting.first().or_else(|| unresolved.first())
                {
                    effective = Some(entry.decl);
                    if self.complete && entry.owes_implementation() {
                        self.obligations.push(entry.decl);
                    }
                }
            }
            self.by_name
                .entry(signature.name().clone())
                .or_default()
                .values
                .push(MemberSlot {
                    candidates: declarations,
                    effective,
                });
        }

        self.obligations.dedup();
        if is_concrete && !self.obligations.is_empty() {
            self.sink.push(SemanticDiagnostic::UnimplementedContract {
                contract: container,
            });
        }
    }

    fn add_unavailable_overrides(&mut self) {
        let facts = &self.facts;
        for entry in facts
            .known
            .values()
            .filter(|entry| OverrideSignature::of(self.db, entry.decl).is_none())
        {
            let def = entry.decl;
            self.declarations.entry(def).or_insert(MemberDeclaration {
                def,
                owner: entry.owner,
                namespace: DefNamespace::from(&def),
            });
            self.by_name
                .entry(def_name(self.db, def).into())
                .or_default()
                .values
                .push(MemberSlot {
                    candidates: Box::new([def]),
                    effective: Some(def),
                });
        }
    }

    fn add_non_override_members(&mut self) {
        let mut index_by_key: FxHashMap<SlotKey, (MemberNamespace, usize)> = FxHashMap::default();

        for &owner in self.mro.iter() {
            let defmap = self.db.file_def_map(owner.file());
            let Some(scope) = defmap.defs.get(&owner).and_then(|data| data.child_scope) else {
                continue;
            };
            let mut declarations: Box<[(Name, DefNamespace, DefId)]> = defmap.scopes[scope]
                .by_name
                .iter()
                .flat_map(|(name, scope_data)| {
                    scope_data
                        .defs
                        .iter()
                        .copied()
                        .filter(|&def| {
                            !matches!(def, DefId::Function(_) | DefId::Modifier(_))
                                && !matches!(
                                    def,
                                    DefId::Var(id) if self.db.var_data(id).vis == Visibility::Public
                                )
                        })
                        .map(|def| (name.clone(), scope_data.namespace, def))
                })
                .collect();
            declarations.sort_by_key(|(_, _, def)| {
                def_range(self.db, *def).map_or(0, |range| range.start)
            });

            for (name, namespace, def) in declarations {
                let key = self.slot_key(name.clone(), namespace, def);
                self.declarations.entry(def).or_insert(MemberDeclaration {
                    def,
                    owner,
                    namespace,
                });
                let is_visible = |d: &DefId| {
                    self.declarations.get(d).is_some_and(|decl| {
                        decl.owner == self.container || !matches!(d, DefId::Var(id) if self.db.var_data(*id).vis == Visibility::Private)
                    })
                };
                let visible = is_visible(&def);
                let group = self.by_name.entry(name.clone()).or_default();
                match key.map(|key| index_by_key.entry(key)) {
                    Some(Entry::Occupied(entry)) => {
                        let (existing_ns, idx) = *entry.get();
                        let slot = &mut group.get_mut(existing_ns)[idx];
                        let mut defs = slot.candidates.to_vec();
                        if !defs.contains(&def) {
                            defs.push(def);
                            match (visible, slot.candidates.iter().any(is_visible)) {
                                (true, true) => {
                                    slot.effective = None;
                                    self.sink.push(
                                        SemanticDiagnostic::InheritedMemberConflict {
                                            contract: self.container,
                                            name,
                                            declarations: defs.iter().filter(|d| is_visible(d)).copied().collect(),
                                        },
                                    );
                                }
                                (true, false) => slot.effective = Some(def),
                                (false, _) => {}
                            }
                            slot.candidates = defs.into_boxed_slice();
                        }
                    }
                    entry => {
                        let member_namespace = MemberNamespace::from(namespace);
                        let slots = group.get_mut(member_namespace);
                        if let Some(Entry::Vacant(entry)) = entry {
                            entry.insert((member_namespace, slots.len()));
                        }
                        slots.push(MemberSlot {
                            candidates: Box::new([def]),
                            effective: visible.then_some(def),
                        });
                    }
                }
            }
        }
    }

    fn slot_key(&self, name: Name, namespace: DefNamespace, def: DefId) -> Option<SlotKey> {
        match def {
            DefId::Event(id) => self
                .db
                .event_signature(id)
                .map(|sig| SlotKey::Overload(name, namespace, sig.params)),
            DefId::Error(id) => self
                .db
                .error_signature(id)
                .map(|sig| SlotKey::Overload(name, namespace, sig.params)),
            _ if matches!(namespace, DefNamespace::Types | DefNamespace::Variables) => {
                Some(SlotKey::NonOverloadable(name))
            }
            _ => None,
        }
    }
}

