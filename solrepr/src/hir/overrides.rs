//! Contract-level override analysis.
//!
//! Builds declaration-to-base override facts and validates own declarations,
//! similar to solc's `OverrideChecker`(`OverrideGraph`, and `CutVertexFinder`). 
//! Effective slots, inherited conflicts, and implementation obligations are folded once by `MemberMap`.

use la_arena::Arena;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::ast::{FunctionKind, HasBases};
use crate::hir::analysis::Analysis;
use crate::hir::exprs::Name;
use crate::hir::item_data::OverrideSpecifier;
use crate::hir::resolution::Resolution;
use crate::hir::resolver::Resolver;
use crate::hir::diagnostics::{DiagnosticSink, SemanticDiagnostic};
use crate::hir::signatures::OverrideSignature;
use crate::hir::types::{TyName, TyNameId, Visibility};
use crate::ir::def_map::DefId;
use crate::salsa::HirDb;
use crate::salsa::root_db::def_range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeclKind {
    Function,
    Modifier,
    Getter,
}

/// One declaration occupying an override slot.
#[derive(Clone, PartialEq, Eq)]
pub struct OverrideEntry {
    pub decl: DefId,
    pub owner: DefId,
    pub kind: DeclKind,
    pub is_virtual: bool,
    pub implemented: bool,
    /// The inherited declarations this one directly overrides.
    pub overrides: Box<[DefId]>,
}

impl OverrideEntry {
    /// An unimplemented declaration is an outstanding obligation — a concrete
    /// contract must be marked `abstract`. Modifiers carry the same obligation
    /// as functions; getters are always implemented.
    pub fn owes_implementation(&self) -> bool {
        matches!(self.kind, DeclKind::Function | DeclKind::Modifier) && !self.implemented
    }
}

/// The declarations occupying one override signature in a contract: its own
/// declarations and the ones inherited from its direct bases.
/// FIXME: currently duplicates OverrideEntry info
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Overrides {
    pub own: Box<[DefId]>,
    pub inherited: Box<[DefId]>,
}

impl Overrides {
    /// The declarations this contract exposes to its children for a signature.
    /// its own when it declares any, else the inherited set.
    pub fn exposed(&self) -> &[DefId] {
        if self.own.is_empty() {
            &self.inherited
        } else {
            &self.own
        }
    }
}

/// Pseudo-nodes used by CutVertex Finder see [`cut_vertices`]
const ROOT: usize = 0;
const TOP: usize = 1;

#[derive(Clone, PartialEq, Eq, Default)]
pub struct OverrideFacts {
    pub by_sig: FxHashMap<OverrideSignature, Overrides>,
    pub known: FxHashMap<DefId, OverrideEntry>,
    pub complete: bool,
}

impl OverrideFacts {
    /// Every declaration occupying `signature` across the contract and its
    /// bases, following override edges from the direct frontier.
    pub fn declarations(&self, signature: &OverrideSignature) -> Box<[DefId]> {
        let Some(overrides) = self.by_sig.get(signature) else {
            return Box::new([]);
        };
        let mut all: Vec<DefId> = overrides.own.iter().chain(&overrides.inherited).copied().collect();
        
        let mut i = 0;
        while i < all.len() {
            if let Some(entry) = self.known.get(&all[i]) {
                for &base in &entry.overrides {
                    if !all.contains(&base) {
                        all.push(base);
                    }
                }
            }
            i += 1;
        }
        all.into_boxed_slice()
    }

    pub fn owes_implementation(&self, def: DefId) -> bool {
        self.known
            .get(&def)
            .is_some_and(OverrideEntry::owes_implementation)
    }

    /// Resolve one inherited slot against the override graph
    /// A cut vertex covers the declarations it overrides. An unimplemented
    /// cut vertex is unresolved, so it still owes an implementation.
    pub fn resolve_slot(
        &self,
        signature: &OverrideSignature,
    ) -> (Box<[&OverrideEntry]>, FxHashSet<DefId>) {
        let Some(overrides) = self.by_sig.get(signature) else {
            return (Box::new([]), FxHashSet::default());
        };
        let candidates = &overrides.inherited;
        let mut graph = SlotGraph::new(&self.known);
        for &decl in candidates {
            let node = graph.visit(decl);
            graph.connect(ROOT, node);
        }

        let mut remaining: FxHashSet<DefId> = candidates.iter().copied().collect();
        let mut unimplemented_cuts = FxHashSet::default();
        let mut stack = Vec::new();
        let mut covered = FxHashSet::default();
        for cut in cut_vertices(&graph.adjacency) {
            let Some(decl) = graph.decls[cut] else {
                continue;
            };
            stack.clear();
            covered.clear();
            stack.extend_from_slice(overrides_of(&self.known, decl));
            while let Some(base) = stack.pop() {
                if covered.insert(base) {
                    remaining.remove(&base);
                    stack.extend_from_slice(overrides_of(&self.known, base));
                }
            }
            if self.known.get(&decl).is_some_and(|entry| !entry.implemented) {
                unimplemented_cuts.insert(decl);
            }
        }
        let unresolved = candidates
            .iter()
            .filter(|decl| remaining.contains(decl))
            .filter_map(|def| self.known.get(def))
            .collect();
        (unresolved, unimplemented_cuts)
    }
}

/// A declaration owned by the analyzed contract, with the facts its checks need.
struct OwnDeclaration {
    def: DefId,
    kind: DeclKind,
    is_virtual: bool,
    implemented: bool,
    marked: bool,
    list: Option<OverrideList>,
}

struct OverrideList {
    /// `None` when a listed path does not resolve to a contract. List
    /// validation is then skipped rather than reporting a guessed mismatch.
    listed: Option<Box<[DefId]>>,
}

pub struct OverrideAnalyzer<'db> {
    db: &'db dyn HirDb,
    resolver: Resolver<'db>,
    container: DefId,
    is_interface: bool,
    complete: bool,
    known: FxHashMap<DefId, OverrideEntry>,
    slots: FxHashMap<OverrideSignature, (Vec<DefId>, Vec<DefId>)>,
    sink: DiagnosticSink,
}

impl<'db> OverrideAnalyzer<'db> {
    pub fn new(db: &'db dyn HirDb, container: DefId) -> Self {
        let resolver = Resolver::for_declaration(db, container);
        Self {
            db,
            resolver,
            container,
            is_interface: matches!(container, DefId::Interface(_)),
            complete: true,
            known: FxHashMap::default(),
            slots: FxHashMap::default(),
            sink: DiagnosticSink::default(),
        }
    }

    pub fn analyze(mut self) -> Analysis<OverrideFacts> {
        if !matches!(self.container, DefId::Contract(_) | DefId::Interface(_)) {
            return Analysis::new(OverrideFacts::default(), self.sink.finish());
        }
        self.collect_bases();
        self.collect_own();
        let by_sig = self
            .slots
            .into_iter()
            .map(|(signature, (own, inherited))| {
                (
                    signature,
                    Overrides {
                        own: own.into_boxed_slice(),
                        inherited: inherited.into_boxed_slice(),
                    },
                )
            })
            .collect();
        Analysis::new(
            OverrideFacts {
                by_sig,
                known: self.known,
                complete: self.complete,
            },
            self.sink.finish(),
        )
    }

    fn collect_bases(&mut self) {
        let (bases, complete) = self.direct_bases();
        self.complete = complete;
        for &base in &bases {
            let base_analysis = self.db.override_facts(base);
            self.complete &= base_analysis.data.complete;
            for (signature, overrides) in &base_analysis.data.by_sig {
                let (_, inherited) = self.slots.entry(signature.clone()).or_default();
                for &declaration in overrides.exposed() {
                    if !inherited.contains(&declaration) {
                        inherited.push(declaration);
                    }
                }
            }
            self.known.extend(
                base_analysis
                    .data
                    .known
                    .iter()
                    .map(|(decl, entry)| (*decl, entry.clone())),
            );
        }
    }

    fn collect_own(&mut self) {
        for declaration in self.own_declarations() {
            let Some(sig) = OverrideSignature::of(self.db, declaration.def) else {
                let entry = OverrideEntry {
                    decl: declaration.def,
                    owner: self.container,
                    kind: declaration.kind,
                    is_virtual: declaration.is_virtual,
                    implemented: declaration.implemented,
                    overrides: Box::new([]),
                };
                self.known.insert(declaration.def, entry);
                continue;
            };
            let inherited: Box<[DefId]> = self
                .slots
                .entry(sig.clone())
                .or_default()
                .1
                .as_slice()
                .into();
            if self.complete {
                self.check_declaration(&declaration, &inherited);
            }
            let (own, _) = self.slots.entry(sig).or_default();
            if let Some(&previous) = own.first() {
                self.sink.push(SemanticDiagnostic::DuplicateMemberSlot {
                    declaration: declaration.def,
                    previous,
                });
            }
            own.push(declaration.def);
            let entry = OverrideEntry {
                decl: declaration.def,
                owner: self.container,
                kind: declaration.kind,
                is_virtual: declaration.is_virtual,
                implemented: declaration.implemented,
                overrides: inherited,
            };
            self.known.insert(declaration.def, entry);
        }
    }

    fn own_declarations(&self) -> Box<[OwnDeclaration]> {
        let defmap = self.db.file_def_map(self.container.file());
        let Some(scope) = defmap
            .defs
            .get(&self.container)
            .and_then(|data| data.child_scope)
        else {
            return Box::new([]);
        };
        let mut defs: Box<[DefId]> = defmap.scopes[scope]
            .by_name
            .values()
            .flat_map(|member| member.defs.iter().copied())
            .collect();
        defs.sort_by_key(|def| def_range(self.db, *def).map_or(0, |range| range.start));
        defs.iter()
            .filter_map(|&def| self.own_declaration(def))
            .collect()
    }

    fn own_declaration(&self, def: DefId) -> Option<OwnDeclaration> {
        match def {
            DefId::Function(id) => {
                let data = self.db.function_data(id);
                if data.kind == FunctionKind::Constructor {
                    return None;
                }
                Some(OwnDeclaration {
                    def,
                    kind: DeclKind::Function,
                    is_virtual: data.is_virtual || self.is_interface,
                    implemented: data.has_body,
                    marked: data.override_specifier.is_some(),
                    list: self.override_list(&data.syntax_store.types, data.override_specifier.as_ref()),
                })
            }
            DefId::Modifier(id) => {
                let data = self.db.modifier_data(id);
                Some(OwnDeclaration {
                    def,
                    kind: DeclKind::Modifier,
                    is_virtual: data.is_virtual,
                    implemented: data.has_body,
                    marked: data.override_specifier.is_some(),
                    list: self.override_list(&data.syntax_store.types, data.override_specifier.as_ref()),
                })
            }
            DefId::Var(id) => {
                let data = self.db.var_data(id);
                if data.vis != Visibility::Public {
                    return None;
                }
                Some(OwnDeclaration {
                    def,
                    kind: DeclKind::Getter,
                    is_virtual: false,
                    implemented: true,
                    marked: data.override_specifier.is_some(),
                    list: self.override_list(&data.syntax_store.types, data.override_specifier.as_ref()),
                })
            }
            _ => None,
        }
    }

    fn override_list(
        &self,
        types: &Arena<TyName>,
        specifier: Option<&OverrideSpecifier>,
    ) -> Option<OverrideList> {
        let OverrideSpecifier::List { paths, .. } = specifier? else {
            return None;
        };
        let listed = paths
            .iter()
            .map(|path| self.listed_contract(types, path.ty_ref))
            .collect::<Option<Box<[_]>>>();
        Some(OverrideList { listed })
    }

    fn listed_contract(&self, types: &Arena<TyName>, ty_ref: Option<TyNameId>) -> Option<DefId> {
        let segments = type_path(types, ty_ref?)?;
        match self.resolver.resolve_path(&segments)? {
            Resolution::Type(ty) => match ty.def_id(self.db)? {
                def @ (DefId::Contract(_) | DefId::Interface(_)) => Some(def),
                _ => None,
            },
            _ => None,
        }
    }

    /// Direct bases in declaration order, and whether every base resolved.
    fn direct_bases(&self) -> (Box<[DefId]>, bool) {
        //@TODO should this be in salsa
        let bases = match self.container {
            DefId::Contract(id) => self
                .db
                .ast_id_map(id.file)
                .get(&self.db.root(id.file), id.id)
                .map(|node| node.bases()),
            DefId::Interface(id) => self
                .db
                .ast_id_map(id.file)
                .get(&self.db.root(id.file), id.id)
                .map(|node| node.bases()),
            _ => None,
        };
        let Some(bases) = bases else {
            return (Box::new([]), false);
        };
        let mut resolved = Vec::with_capacity(bases.len());
        let mut complete = true;
        for base in bases.iter() {
            match self.resolver.resolve_path(&base.segments) {
                Some(Resolution::Type(ty)) => match ty.def_id(self.db) {
                    Some(def @ (DefId::Contract(_) | DefId::Interface(_))) => resolved.push(def),
                    _ => complete = false,
                },
                _ => complete = false,
            }
        }
        (resolved.into_boxed_slice(), complete)
    }

    fn check_declaration(&mut self, declaration: &OwnDeclaration, inherited: &[DefId]) {
        let def = declaration.def;
        let candidates: Box<[&OverrideEntry]> = inherited
            .iter()
            .filter_map(|def| self.known.get(def))
            .collect();
        if candidates.is_empty() {
            if declaration.marked {
                self.sink.push(SemanticDiagnostic::UnnecessaryOverride {
                    declaration: def,
                });
            }
            return;
        }

        if let Some(base) = candidates
            .iter()
            .find(|candidate| candidate.kind != DeclKind::Getter && !candidate.is_virtual)
        {
            self.sink.push(SemanticDiagnostic::NotVirtual {
                declaration: def,
                base: base.decl,
            });
        }

        let incompatible = candidates.iter().find_map(|candidate| {
            let mismatch = OverrideSignature::compatibility(self.db, def, candidate.decl)?
                .first()
                .copied()?;
            Some((candidate.decl, mismatch))
        });
        if let Some((base, mismatch)) = incompatible {
            self.sink.push(SemanticDiagnostic::IncompatibleOverride {
                declaration: def,
                base,
                mismatch,
            });
        }

        let overridable = |kind: DeclKind| matches!(kind, DeclKind::Function | DeclKind::Modifier);
        if overridable(declaration.kind)
            && !declaration.implemented
            && let Some(base) = candidates
                .iter()
                .find(|candidate| overridable(candidate.kind) && candidate.implemented)
        {
            self.sink.push(SemanticDiagnostic::UnimplementedOverride {
                declaration: def,
                base: base.decl,
            });
        }

        let mut owners: Vec<DefId> = Vec::new();
        for candidate in &candidates {
            if !owners.contains(&candidate.owner) {
                owners.push(candidate.owner);
            }
        }
        let single_interface = owners.len() == 1 && matches!(owners[0], DefId::Interface(_));
        if !declaration.marked && !single_interface {
            self.sink.push(SemanticDiagnostic::MissingOverride {
                declaration: def,
                bases: candidates.iter().map(|candidate| candidate.decl).collect(),
            });
        }

        if let Some(OverrideList {
            listed: Some(listed),
        }) = &declaration.list
        {
            let distinct: FxHashSet<DefId> = listed.iter().copied().collect();
            let owner_set: FxHashSet<DefId> = owners.iter().copied().collect();
            if distinct.len() != listed.len() || distinct != owner_set {
                self.sink.push(SemanticDiagnostic::OverrideListMismatch {
                    declaration: def,
                    expected: owners,
                });
            }
        }
    }
}

fn type_path(types: &Arena<TyName>, id: TyNameId) -> Option<Vec<Name>> {
    match &types[id] {
        TyName::UserDefined { base, name } => {
            let mut path = match base {
                Some(base) => type_path(types, *base)?,
                None => Vec::new(),
            };
            path.push(name.clone());
            Some(path)
        }
        TyName::Located { ty, .. } => type_path(types, *ty),
        _ => None,
    }
}

fn overrides_of(known: &FxHashMap<DefId, OverrideEntry>, decl: DefId) -> &[DefId] {
    known.get(&decl).map_or(&[], |entry| &entry.overrides)
}

/// Solc's `OverrideGraph` for one inherited slot: node `0` (`ROOT`) is the current contract,
/// node `1` (`TOP`) is an artificial top node that every unoverridden declaration connects to,
/// and each declaration gets its own node. Edges are undirected.
struct SlotGraph<'a> {
    known: &'a FxHashMap<DefId, OverrideEntry>,
    nodes: FxHashMap<DefId, usize>,
    decls: Vec<Option<DefId>>,
    adjacency: Vec<Vec<usize>>,
}

impl<'a> SlotGraph<'a> {
    fn new(known: &'a FxHashMap<DefId, OverrideEntry>) -> Self {
        Self {
            known,
            nodes: FxHashMap::default(),
            decls: vec![None, None],
            adjacency: vec![Vec::new(), Vec::new()],
        }
    }

    fn connect(&mut self, a: usize, b: usize) {
        self.adjacency[a].push(b);
        self.adjacency[b].push(a);
    }

    fn visit(&mut self, decl: DefId) -> usize {
        if let Some(node) = self.nodes.get(&decl) {
            return *node;
        }
        let node = self.decls.len();
        self.decls.push(Some(decl));
        self.adjacency.push(Vec::new());
        self.nodes.insert(decl, node);

        let overrides = overrides_of(self.known, decl);
        if overrides.is_empty() {
            self.connect(node, TOP);
        } else {
            for &base in overrides {
                let base_node = self.visit(base);
                self.connect(node, base_node);
            }
        }
        node
    }
}

/// Articulation points (Hopcroft–Tarjan) of the slot graph, excluding `ROOT`
/// and `TOP`. Ported from `CutVertexFinder` in solc's
/// `libsolidity/analysis/OverrideChecker.cpp`, which follows
/// <https://en.wikipedia.org/wiki/Biconnected_component#Pseudocode>. A
/// declaration that is a cut vertex covers everything it overrides.
fn cut_vertices(adjacency: &[Vec<usize>]) -> FxHashSet<usize> {
    struct CvF<'a> {
        adjacency: &'a [Vec<usize>],
        depth: Box<[Option<usize>]>,
        low: Box<[usize]>,
        parent: Box<[Option<usize>]>,
        cuts: FxHashSet<usize>,
    }

    impl CvF<'_> {
        fn run(&mut self, u: usize, depth: usize) {
            let adjacency = self.adjacency;
            self.depth[u] = Some(depth);
            self.low[u] = depth;
            for &v in &adjacency[u] {
                match self.depth[v] {
                    None => {
                        self.parent[v] = Some(u);
                        self.run(v, depth + 1);
                        if self.low[v] >= depth && self.parent[u].is_some() {
                            self.cuts.insert(u);
                        }
                        self.low[u] = self.low[u].min(self.low[v]);
                    }
                    Some(v_depth) if Some(v) != self.parent[u] => {
                        self.low[u] = self.low[u].min(v_depth);
                    }
                    Some(_) => {}
                }
            }
        }
    }

    let n = adjacency.len();
    let mut cvf = CvF {
        adjacency,
        depth: vec![None; n].into_boxed_slice(),
        low: vec![0; n].into_boxed_slice(),
        parent: vec![None; n].into_boxed_slice(),
        cuts: FxHashSet::default(),
    };
    cvf.run(ROOT, 0);
    cvf.cuts.remove(&TOP);
    cvf.cuts
}


