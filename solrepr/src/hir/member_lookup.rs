use std::collections::{BTreeMap, BTreeSet};

use crate::hir::access::{AccessDenial, MemberScan};
use crate::hir::builtins::MemberNamespace;
use crate::hir::exprs::Name;
use crate::hir::member_map::{MemberMap, MemberSlot};
use crate::hir::resolution::{Callable, Field, Resolution, Variant};
use crate::hir::resolver::Resolver;
use crate::hir::types::{BaseKind, CallKind, Ty, TyKind, ViaTypeKind};
use crate::ir::def_map::{DefId, ScopeData, TypeDefId};
use crate::salsa::File;

/// The projected outcome of looking up one member name on a receiver.
///
/// Effective declarations are split into callables and plain values;
/// conflict-visible declarations and access denials are kept alongside so a
/// caller can distinguish "nothing matched" from "the match was rejected"
/// and retain navigation targets on conflicted lookups.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct MemberLookupResult {
    /// Effective callables — functions, getters, bound `using` functions,
    /// builtin functions — kept parallel for overload selection.
    pub callables: Vec<Callable>,
    /// Effective non-callable resolutions — fields, state variables, types,
    /// enum variants, modules, builtin objects.
    pub values: Vec<Resolution>,
    /// Visible declarations from conflicted slots — retained for completion
    /// and conflict diagnostics, never dispatch candidates.
    pub conflicts: Vec<Resolution>,
    /// Every declaration of each conflicted slot the name reached, whether
    /// or not access lets it through: the links of an ambiguity diagnostic.
    pub ambiguous: Vec<DefId>,
    /// Access denials recorded while projecting this name.
    pub denials: Vec<AccessDenial>,
}

impl MemberLookupResult {
    /// Anything matched the name — effective or conflict-visible.
    pub fn is_hit(&self) -> bool {
        !self.callables.is_empty() || !self.values.is_empty() || !self.conflicts.is_empty()
    }

    /// A callable and a plain value share the name (a `using` method
    /// colliding with a field): the member access is ambiguous.
    pub fn is_ambiguous_value(&self) -> bool {
        !self.callables.is_empty() && !self.values.is_empty()
    }

    /// The resolution an expression gets: callables win only when the name
    /// is not also a non-callable value; otherwise a sole value can resolve.
    pub fn effective_resolution(&self) -> Option<Resolution> {
        match (self.callables.as_slice(), self.values.as_slice()) {
            ([], [value]) => Some(value.clone()),
            ([callable], []) => Some(Resolution::Callable(*callable)),
            (callables @ [_, ..], []) => Some(Resolution::Callables(callables.into())),
            _ => None,
        }
    }

    /// Every resolution worth offering for this name — effective and
    /// conflict-visible, with callables kept separate.
    pub fn completion_resolutions(&self) -> Vec<Resolution> {
        self.callables
            .iter()
            .cloned()
            .map(Resolution::Callable)
            .chain(self.values.iter().cloned())
            .chain(self.conflicts.iter().cloned())
            .collect()
    }

    fn push_effective(&mut self, resolution: Resolution) {
        match resolution {
            Resolution::Callable(callable) => self.callables.push(callable),
            Resolution::Callables(callables) => self.callables.extend(callables.into_vec()),
            value => self.values.push(value),
        }
    }
}

impl<'db> Resolver<'db> {
    /// The projected outcome for `name` alone — indexes each source's
    /// `by_name` table directly with no map allocation.
    pub fn member_named(&self, receiver: Ty, name: &Name) -> MemberLookupResult {
        let mut result = MemberLookupResult::default();
        match receiver.kind(self.db) {
            TyKind::Type(inner) => self.types_member_named(inner.ty(), name, &mut result),
            TyKind::Module(file) => self.file_member_named(file, name, &mut result),
            TyKind::Builtin(_) | TyKind::Meta(_) => {
                self.add_builtins_named(&mut result, receiver, MemberNamespace::Types, name);
            }
            TyKind::Super(id) => {
                self.super_member_named(DefId::Contract(id), name, &mut result);
            }
            _ => self.values_member_named(receiver, name, &mut result),
        }
        result
    }

    /// Declaration-context member lookup — the declaring scope only, so
    /// header signatures never depend on the effective-member fold.
    pub fn declaration_member_named(&self, receiver: Ty, name: &Name) -> MemberLookupResult {
        if let TyKind::Type(inner) = receiver.kind(self.db)
            && let Some(owner @ (DefId::Contract(_) | DefId::Interface(_) | DefId::Library(_))) =
                inner.def_id(self.db)
        {
            let mut result = MemberLookupResult::default();
            self.own_scope_member(&mut result, owner, name, self.access.type_access_kind(owner));
            return result;
        }
        self.member_named(receiver, name)
    }

    /// The projected outcome `owner`'s scope contributes for `name` —
    /// including inherited slots for contract owners.
    pub fn direct_member_named(&self, owner: DefId, name: &Name) -> MemberLookupResult {
        let mut result = MemberLookupResult::default();
        match owner {
            DefId::Contract(_) | DefId::Interface(_) => {
                self.contract_member_named(owner, CallKind::Direct, true, false, name, &mut result);
            }
            DefId::Library(_) => {
                self.own_scope_member(&mut result, owner, name, CallKind::Direct);
            }
            _ => {}
        }
        result
    }

    /// Declaration-context counterpart of `direct_member_named`: the owner's
    /// declaring scope, then its bases in linearization order, without the
    /// effective-member fold. Only the owner's own scope is consulted when it
    /// hits, so the bases query is not demanded for locally declared names.
    pub fn declaration_direct_member_named(&self, owner: DefId, name: &Name) -> MemberLookupResult {
        let mut result = MemberLookupResult::default();
        self.own_scope_member(&mut result, owner, name, CallKind::Direct);
        if result.is_hit() || !matches!(owner, DefId::Contract(_) | DefId::Interface(_)) {
            return result;
        }
        for &base in self.db.bases(owner).iter().skip(1) {
            self.own_scope_member(&mut result, base, name, CallKind::Direct);
            if result.is_hit() {
                break;
            }
        }
        result
    }

    fn own_scope_member(
        &self,
        result: &mut MemberLookupResult,
        owner: DefId,
        name: &Name,
        path: CallKind,
    ) {
        let defmap = self.db.file_def_map(owner.file());
        if let Some(scope) = defmap.defs.get(&owner).and_then(|data| data.child_scope)
            && let Some(member) = defmap.scopes[scope].by_name.get(name)
        {
            self.scope_member(result, owner, member, path);
        }
    }

    /// Every member `receiver` exposes — collects the candidate names and
    /// delegates each to `member_named`.
    pub fn members_of(&self, receiver: Ty) -> BTreeMap<Name, MemberLookupResult> {
        let mut names = BTreeSet::new();
        self.receiver_names(receiver, &mut names);
        names
            .into_iter()
            .map(|name| {
                let result = self.member_named(receiver, &name);
                (name, result)
            })
            .filter(|(_, result)| result.is_hit())
            .collect()
    }

    /// Every member `owner`'s scope contributes — for bare-name completion.
    pub fn direct_members_of(&self, owner: DefId) -> BTreeMap<Name, MemberLookupResult> {
        let mut names = BTreeSet::new();
        if matches!(owner, DefId::Contract(_) | DefId::Interface(_) | DefId::Library(_)) {
            self.scope_names(owner, &mut names);
        }
        names
            .into_iter()
            .map(|name| {
                let result = self.direct_member_named(owner, &name);
                (name, result)
            })
            .filter(|(_, result)| result.is_hit())
            .collect()
    }

    fn scope_names(&self, owner: DefId, names: &mut BTreeSet<Name>) {
        if matches!(owner, DefId::Contract(_) | DefId::Interface(_)) {
            names.extend(self.db.member_map(owner).data.by_name.keys().cloned());
        } else {
            let defmap = self.db.file_def_map(owner.file());
            if let Some(scope) = defmap.defs.get(&owner).and_then(|data| data.child_scope) {
                names.extend(defmap.scopes[scope].by_name.keys().cloned());
            }
        }
    }

    fn receiver_names(&self, receiver: Ty, names: &mut BTreeSet<Name>) {
        match receiver.kind(self.db) {
            TyKind::Type(inner) => match inner.peel_loc(self.db).kind(self.db) {
                TyKind::Module(file) => {
                    for (defmap, scope) in self.file_scopes(file) {
                        names.extend(defmap.scopes[scope].by_name.keys().cloned());
                    }
                }
                TyKind::TypeDef(id) if id.is_contract_like() => {
                    self.scope_names(id.into(), names);
                }
                TyKind::TypeDef(TypeDefId::Enum(enum_id)) => {
                    for (_, variant) in self.db.enum_data(enum_id).variants.iter() {
                        names.insert(variant.name.clone());
                    }
                }
                _ => {
                    for builtin in self.builtins().members(inner.ty(), MemberNamespace::Types).iter() {
                        names.insert(builtin.name().into());
                    }
                }
            },
            TyKind::Module(file) => {
                for (defmap, scope) in self.file_scopes(file) {
                    names.extend(defmap.scopes[scope].by_name.keys().cloned());
                }
            }
            TyKind::Builtin(_) | TyKind::Meta(_) => {
                for builtin in self.builtins().members(receiver, MemberNamespace::Types).iter() {
                    names.insert(builtin.name().into());
                }
            }
            TyKind::Super(id) => self.scope_names(DefId::Contract(id), names),
            _ => {
                let base = receiver.peel_loc(self.db);
                match base.kind(self.db) {
                    TyKind::TypeDef(TypeDefId::Struct(id)) => {
                        for (_, data) in self.db.struct_data(id).fields.iter() {
                            names.insert(data.name.clone());
                        }
                    }
                    TyKind::TypeDef(id)
                        if matches!(id, TypeDefId::Contract(_) | TypeDefId::Interface(_)) =>
                    {
                        self.scope_names(id.into(), names);
                    }
                    _ => {}
                }
                for builtin in self.builtins().members(receiver, MemberNamespace::Values).iter() {
                    names.insert(builtin.name().into());
                }
                let index = self.db.using_index(self.access.caller());
                if let Some(exact) = index.exact.get(&base) {
                    names.extend(exact.keys().cloned());
                }
                names.extend(index.any.keys().cloned());
            }
        }
    }

    fn types_member_named(&self, ty: Ty, name: &Name, result: &mut MemberLookupResult) {
        match ty.peel_loc(self.db).kind(self.db) {
            TyKind::Module(file) => self.file_member_named(file, name, result),
            TyKind::TypeDef(id) if id.is_contract_like() => {
                let owner = id.into();
                let path = self.access.type_access_kind(owner);
                // Members declared in the type itself — functions, modifiers,
                // events, errors, constants and nested types. Member access on
                // a contract type is declared-only in solc: `B.E`/`B.f` do not
                // reach members `B` merely inherits (inherited *type* members
                // are still reachable in type positions via `lookup_type`).
                if matches!(owner, DefId::Contract(_) | DefId::Interface(_)) {
                    self.contract_member_named(owner, path, true, true, name, result);
                } else {
                    self.own_scope_member(result, owner, name, path);
                }
            }
            TyKind::TypeDef(TypeDefId::Enum(enum_id)) => {
                let data = self.db.enum_data(enum_id);
                for (id, variant) in data.variants.iter() {
                    if variant.name == *name {
                        result.push_effective(Resolution::Variant(Variant::new(enum_id, id)));
                    }
                }
            }
            // UDVT `wrap`/`unwrap` and primitive type namespaces
            // (`bytes.concat`, `string.concat`) are builtin-backed.
            _ => self.add_builtins_named(result, ty, MemberNamespace::Types, name),
        }
    }

    /// Members visible on a contract/interface value: its external
    /// interface. Public/external functions become externally-accessed
    /// callables, public state variables become getters, and internal/private
    /// members are not reachable through a value.
    fn values_member_named(&self, ty: Ty, name: &Name, result: &mut MemberLookupResult) {
        let base = ty.peel_loc(self.db);
        match base.kind(self.db) {
            TyKind::TypeDef(TypeDefId::Struct(id)) => {
                let owner = DefId::Struct(id);
                for (field, data) in self.db.struct_data(id).fields.iter() {
                    if data.name == *name {
                        result.push_effective(Resolution::Field(Field::new_struct(owner, field)));
                    }
                }
            }
            TyKind::TypeDef(id)
                if matches!(id, TypeDefId::Contract(_) | TypeDefId::Interface(_)) =>
            {
                let owner = id.into();
                self.contract_member_named(
                    owner,
                    CallKind::External,
                    false,
                    false,
                    name,
                    result,
                );
            }
            _ => {}
        }
        self.add_builtins_named(result, ty, MemberNamespace::Values, name);
        self.add_using_named(result, ty, name);
    }

    fn contract_member_named(
        &self,
        owner: DefId,
        path: CallKind,
        include_types: bool,
        declared_only: bool,
        name: &Name,
        result: &mut MemberLookupResult,
    ) {
        let members = self.db.member_map(owner).data;
        let Some(group) = members.by_name.get(name) else {
            return;
        };
        let slots = if include_types {
            group.types.iter().chain(&group.values)
        } else {
            [].iter().chain(&group.values)
        };
        for slot in slots {
            if declared_only
                && !slot.candidates.iter().any(|declaration| {
                    members
                        .declaration(*declaration)
                        .is_some_and(|declaration| declaration.owner == owner)
                })
            {
                continue;
            }
            self.slot_candidate(result, &members, path, slot);
        }
    }

    fn slot_candidate(
        &self,
        result: &mut MemberLookupResult,
        members: &MemberMap,
        path: CallKind,
        slot: &MemberSlot,
    ) {
        match slot.effective {
            // A denied declaration records its denials while projecting and
            // yields no hit — it does not shadow the scopes lookup continues
            // into.
            Some(def) => {
                if let Some(resolution) = self.project_def(result, members, def, path) {
                    result.push_effective(resolution);
                }
            }
            None => {
                let before = result.conflicts.len();
                for &def in &slot.candidates {
                    if let Some(resolution) = self.project_def(result, members, def, path) {
                        result.conflicts.push(resolution);
                    }
                }
                if result.conflicts.len() > before {
                    result.ambiguous.extend_from_slice(&slot.candidates);
                }
            }
        }
    }

    fn project_def(
        &self,
        result: &mut MemberLookupResult,
        members: &MemberMap,
        def: DefId,
        path: CallKind,
    ) -> Option<Resolution> {
        let declaration = members.declaration(def)?;
        match self
            .access
            .scan(path, declaration.owner, &declaration.namespace, &[def])
        {
            MemberScan::Hit { .. } => Some(self.resolution_for_def(def, declaration.owner, path)),
            MemberScan::Denied(denials) => {
                result.denials.extend(denials);
                None
            }
            MemberScan::Skip => None,
        }
    }

    /// Apply the namespace-aware access policy to one declaration group.
    /// `owner` is the scope the definitions were read from; `caller` is
    /// the lexical caller.
    fn scope_member(
        &self,
        result: &mut MemberLookupResult,
        owner: DefId,
        member: &ScopeData,
        path: CallKind,
    ) {
        match self
            .access
            .scan(path, owner, &member.namespace, &member.defs)
        {
            MemberScan::Hit { defs, .. } => {
                for def in defs {
                    result.push_effective(self.resolution_for_def(def, owner, path));
                }
            }
            MemberScan::Denied(denials) => result.denials.extend(denials),
            MemberScan::Skip => {}
        }
    }

    fn resolution_for_def(&self, def: DefId, owner: DefId, path: CallKind) -> Resolution {
        match def {
            DefId::File(file) => Resolution::Module(file),
            DefId::Contract(_)
            | DefId::Interface(_)
            | DefId::Library(_)
            | DefId::Struct(_)
            | DefId::Enum(_)
            | DefId::Udvt(_) => Resolution::from_type(self.db, def),
            DefId::Var(id) if path == CallKind::External => {
                Resolution::Callable(Callable::new_getter(id))
            }
            DefId::Var(_) => Resolution::Var(def),
            DefId::Function(_) => {
                Resolution::Callable(Callable::new_direct(def).with_kind(path))
            }
            DefId::Error(_) | DefId::Event(_) | DefId::Modifier(_) => {
                let path = if matches!(path, CallKind::Base(_)) {
                    CallKind::ViaType(ViaTypeKind::from_owner(owner))
                } else {
                    path
                };
                Resolution::Callable(Callable::new_direct(def).with_kind(path))
            }
            _ => Resolution::Var(def),
        }
    }

    /// Members visible through `super`: functions only. `super.f` selects
    /// the first *implemented* declaration found while walking the
    /// declaring contract's own C3 linearization after the contract itself.
    /// The merged slot's outcome does not govern dispatch — except a
    /// conflicted slot, which never produced a dispatch order and stays a
    /// conflict candidate.
    fn super_member_named(&self, owner: DefId, name: &Name, result: &mut MemberLookupResult) {
        let members = self.db.member_map(owner).data;
        let Some(group) = members.by_name.get(name) else {
            return;
        };
        for slot in &group.values {
            // `slot.declarations` is already ordered by `owner`'s C3
            // linearization; skip `owner`'s own declarations.
            let base_declarations = slot.candidates.iter().copied().filter(|&def| {
                members
                    .declaration(def)
                    .is_some_and(|declaration| declaration.owner != owner)
            });
            if slot.effective.is_none() {
                let declarations: Vec<_> = base_declarations
                    .filter(|def| matches!(def, DefId::Function(_)))
                    .collect();
                let before = result.conflicts.len();
                for &def in &declarations {
                    if let Some(resolution) =
                        self.project_def(result, &members, def, CallKind::Base(BaseKind::Super))
                    {
                        result.conflicts.push(resolution);
                    }
                }
                if result.conflicts.len() > before {
                    result.ambiguous.extend(declarations);
                }
            } else {
                for def in base_declarations {
                    let DefId::Function(id) = def else {
                        continue;
                    };
                    if let Some(resolution) =
                        self.project_def(result, &members, def, CallKind::Base(BaseKind::Super))
                        && self.db.function_data(id).has_body
                    {
                        result.push_effective(resolution);
                        break;
                    }
                }
            }
        }
    }

    fn file_member_named(&self, file: File, name: &Name, result: &mut MemberLookupResult) {
        for (defmap, scope) in self.file_scopes(file) {
            if let Some(member) = defmap.scopes[scope].by_name.get(name) {
                for &def in &member.defs {
                    result.push_effective(self.resolution_for_def(
                        def,
                        self.access.caller(),
                        CallKind::Direct,
                    ));
                }
            }
        }
    }

    fn add_builtins_named(
        &self,
        result: &mut MemberLookupResult,
        ty: Ty,
        namespace: MemberNamespace,
        name: &Name,
    ) {
        for builtin in self.builtins().members(ty, namespace).iter() {
            if builtin.name() == name.as_str() {
                result.push_effective(Resolution::from(*builtin));
            }
        }
    }

    fn add_using_named(&self, result: &mut MemberLookupResult, receiver: Ty, name: &Name) {
        let index = self.db.using_index(self.access.caller());
        // `using T for` keys are peeled — peel the receiver to match.
        let key = receiver.peel_loc(self.db);
        let exact = index
            .exact
            .get(&key)
            .and_then(|members| members.get(name))
            .into_iter()
            .flatten()
            .copied();
        let any = index.any.get(name).into_iter().flatten().copied();
        let mut seen = rustc_hash::FxHashSet::default();
        for def in exact.chain(any) {
            if !seen.insert(def) || !self.using_candidate_accepts(def, receiver) {
                continue;
            }
            result.push_effective(Resolution::Callable(Callable::new_bound(def)));
        }
    }

    /// Whether `receiver` is implicitly convertible to a `using` function's
    /// first (bound) parameter.
    fn using_candidate_accepts(&self, def: DefId, receiver: Ty) -> bool {
        let DefId::Function(id) = def else {
            return false;
        };
        let Some(sig) = self.db.function_signature(id) else {
            return false;
        };
        sig.params
            .first()
            .is_some_and(|first| receiver.implicitly_converts_to(*first, self.db).is_some())
    }
}
