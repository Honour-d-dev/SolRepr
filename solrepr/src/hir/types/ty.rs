use num_bigint::BigInt;

use crate::hir::types::func::Fn;
use crate::hir::types::kind::{Tuple, TyKind};
use crate::hir::types::literal::LiteralKind;
use crate::hir::types::primitive::Primitive;
use crate::hir::types::syntax::{Location, RefKind, VariableKind};
use crate::ir::def_map::DefId;
use crate::salsa::HirDb;

// ── Semantic types ──────────────────────────────────────────────────────────

/// Every expression's type (E) — the interned type universe.
#[salsa::interned(no_lifetime)]
pub struct Ty {
    pub kind: TyKind,
}

impl Ty {
    pub fn invalid(db: &dyn HirDb) -> Ty {
        Ty::new(db, TyKind::Invalid)
    }

    pub fn void(db: &dyn HirDb) -> Ty {
        Ty::new(db, TyKind::Void)
    }

    pub fn primitive(db: &dyn HirDb, primitive: Primitive) -> Ty {
        match primitive {
            Primitive::Undefined => Ty::invalid(db),
            _ => Type::primitive(db, primitive).ty(),
        }
    }

    /// Packs a type list into a single `Ty` — a single element is its own
    /// type, an empty list is `void`, two or more is a tuple. Shared by
    /// function results and multi-value decode positions.
    pub fn from_types(db: &dyn HirDb, types: &[Type]) -> Ty {
        match types {
            [] => Ty::void(db),
            [single] => single.ty(),
            _ => Ty::tuple(db, types.iter().copied().collect())
                .expect("a multi-element type list is a tuple"),
        }
    }

    /// A tuple of at least two declared types, none omitted — a multi-value
    /// return. A single return is its own type, not a tuple.
    pub fn tuple(db: &dyn HirDb, elems: Box<[Type]>) -> Option<Ty> {
        (elems.len() >= 2)
            .then(|| Ty::tuple_from(db, elems.into_iter().map(|elem| Some(elem.ty())).collect()))
    }

    /// A tuple from raw components; `None` is an omitted component. Used where
    /// a tuple expression may contain holes, literals, or nested tuples.
    pub fn tuple_from(db: &dyn HirDb, elements: Box<[Option<Ty>]>) -> Ty {
        Ty::new(db, TyKind::Tuple(Tuple { elements }))
    }

    /// Downcast to the declarable subset.
    pub fn narrow(self, db: &dyn HirDb) -> Option<Type> {
        self.is_nameable(db).then_some(Type(self))
    }

    fn is_nameable(self, db: &dyn HirDb) -> bool {
        self.kind(db).is_nameable()
    }

    pub fn is_invalid(self, db: &dyn HirDb) -> bool {
        matches!(self.kind(db), TyKind::Invalid)
    }

    pub fn def_id(self, db: &dyn HirDb) -> Option<DefId> {
        self.peel_loc(db).kind(db).def_id()
    }

    pub fn peel_loc(self, db: &dyn HirDb) -> Ty {
        let peeled = match self.kind(db) {
            TyKind::Ref(inner, _) => inner.ty(),
            _ => self,
        };
        debug_assert!(
            !matches!(peeled.kind(db), TyKind::Ref(..)),
            "double reference type found"
        );
        peeled
    }

    pub fn loc(self, db: &dyn HirDb) -> Option<Location> {
        match self.kind(db) {
            TyKind::Ref(_, loc) => Some(loc),
            TyKind::Slice(base) => base.loc(db),
            _ => None,
        }
    }

    /// The location this kind defaults to when a slot requires one. A located
    /// type reports its kind's default, not its current location.
    pub fn default_loc(self, db: &dyn HirDb) -> Option<Location> {
        match self.kind(db) {
            TyKind::Ref(inner, _) => inner.default_loc(db).or(Some(Location::Memory)),
            kind => kind.default_loc(),
        }
    }

    /// `Type::at_loc`, projected onto the full universe: non-nameable kinds
    /// are returned unchanged.
    pub fn at_loc(self, db: &dyn HirDb, loc: Location) -> Ty {
        self.narrow(db).map_or(self, |ty| ty.at_loc(db, loc).ty())
    }

    /// `Type::upcast`, projected onto the full universe: non-nameable kinds
    /// are returned unchanged.
    pub fn upcast(self, db: &dyn HirDb) -> Ty {
        self.narrow(db).map_or(self, |ty| ty.upcast(db).ty())
    }

    /// `Type::upcast_from`, projected onto the full universe: non-nameable
    /// kinds are returned unchanged.
    pub fn upcast_from(self, db: &dyn HirDb, loc: Location) -> Ty {
        self.narrow(db)
            .map_or(self, |ty| ty.upcast_from(db, loc).ty())
    }

    /// `Type`'s declared-kind upcast, projected onto the full universe:
    /// state variables bind `storage`, everything else takes its default.
    pub fn upcast_from_kind(self, db: &dyn HirDb, kind: VariableKind) -> Ty {
        match kind {
            VariableKind::StateVariable => self.at_loc(db, Location::Storage(RefKind::Reference)),
            _ => self.upcast(db),
        }
    }

    pub fn implicitly_converts_to(self, target: impl Into<Ty>, db: &dyn HirDb) -> Option<u8> {
        self.kind(db)
            .implicitly_converts_to(&target.into().kind(db), db)
    }

    /// solc's `Type::mobileType()`: the concrete type a literal defaults to
    /// when its context does not name one, and the type itself otherwise.
    /// A literal's mobile type is the smallest type that can hold it 
    pub fn mobile_ty(self, db: &dyn HirDb) -> Ty {
        match self.kind(db) {
            TyKind::Literal(literal) => match literal.inferred_type() {
                Some(kind) => Ty::new(db, kind).upcast(db),
                None => self,
            },
            // `ArraySliceType::mobileType`: the array itself when the slice is
            // a legal one; the slice type otherwise, so a bad slice still
            // can't be placed by a common-type deduction.
            TyKind::Slice(base) => {
                let base = base.ty();
                let base_loc = base.loc(db);
                let base_kind = base.peel_loc(db).kind(db);
                let has_dynamic_elements = match base_kind {
                    TyKind::Array { elem, .. } => elem.is_dynamically_encoded(db),
                    _ => false,
                };
                if base_loc == Some(Location::Calldata) && base_kind.is_dynamically_sized() && !has_dynamic_elements {
                    base
                } else {
                    self
                }
            }
            _ => self,
        }
    }

    /// solc's `Type::commonType` — the type two expression positions share:
    /// a ternary's branches, an inline array's elements.
    pub fn common_ty(db: &dyn HirDb, one: Ty, other: Ty, ) -> Option<Ty> {
        let mobile = one.mobile_ty(db);
        if other.implicitly_converts_to(mobile, db).is_some() {
            return Some(mobile);
        }
        let other_mobile = other.mobile_ty(db);
        if one.implicitly_converts_to(other_mobile, db).is_some() {
            return Some(other_mobile);
        }
        one.fractional_literal_fold(other, db)
    }

    /// `LiteralKind::Rational` has no declarable `mobile_ty` and nothing converts to a literal,
    /// So `common_ty` with numeric literal pairs involving a fraction (`1` and `1.5`, `1.5` and `2.5`) 
    /// collapse to a Rational  instead of none.
    fn fractional_literal_fold(self, other: Ty, db: &dyn HirDb) -> Option<Ty> {
        let (TyKind::Literal(this), TyKind::Literal(that)) =
            (self.kind(db), other.kind(db))
        else {
            return None;
        };
        let numeric = |literal: LiteralKind| {
            matches!(literal, LiteralKind::Int { .. } | LiteralKind::Rational)
        };
        if !numeric(this) || !numeric(that) {
            return None;
        }
        match (this, that) {
            (LiteralKind::Rational, _) => Some(self),
            (_, LiteralKind::Rational) => Some(other),
            _ => None,
        }
    }

    
    pub fn is_dynamically_encoded(self, db: &dyn HirDb) -> bool {
        match self.kind(db) {
            TyKind::Primitive(Primitive::Bytes | Primitive::String) => true,
            TyKind::Array { elem, size } => {
                size.is_none() || elem.is_dynamically_encoded(db)
            }
            TyKind::Ref(inner, _) => inner.is_dynamically_encoded(db),
            TyKind::Tuple(tuple) => tuple
                .elements
                .iter()
                .flatten()
                .any(|elem| elem.is_dynamically_encoded(db)),
            TyKind::Struct(id) => db
                .inference(DefId::Struct(id))
                .fields
                .iter()
                .any(|(_, field)| field.is_dynamically_encoded(db)),
            _ => false,
        }
    }

    pub fn explicitly_converts_to(self, target: Type, db: &dyn HirDb) -> Option<u8> {
        self.kind(db).explicitly_converts_to(&target.kind(db), db)
    }

    pub fn display(self, db: &dyn HirDb) -> String {
        self.kind(db).display(db)
    }
}

/// A declarable subset of `Ty`.
/// A Type that is declarable programatically e.g. uint, address etc
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Type(pub(crate) Ty);

impl From<Type> for Ty {
    fn from(ty: Type) -> Ty {
        ty.0
    }
}

impl Type {
    /// Every constructed type carries its default data location. Compound
    /// interiors stay canonical (`array`/`mapping` peel them), so `Ref`
    /// appears once, at the top of a located slot.
    pub fn new(db: &dyn HirDb, kind: TyKind) -> Option<Type> {
        kind.is_nameable().then(|| Type(Ty::new(db, kind)).upcast(db))
    }

    pub fn invalid(db: &dyn HirDb) -> Type {
        Type::new(db, TyKind::Invalid).expect("`Invalid` is nameable")
    }

    pub fn primitive(db: &dyn HirDb, primitive: Primitive) -> Type {
        Type::new(db, TyKind::Primitive(primitive)).expect("primitives are nameable")
    }

    /// Compound interiors are stored peeled: `Ref` appears once, at the top of
    /// a located slot, never inside `Array { elem }` or `Mapping { value }`.
    pub fn array(db: &dyn HirDb, elem: Type, size: Option<BigInt>) -> Type {
        Type::new(db, TyKind::Array { elem: elem.peel_loc(db), size })
            .expect("arrays are nameable")
    }

    pub fn mapping(db: &dyn HirDb, key: Type, value: Type) -> Type {
        Type::new(db, TyKind::Mapping {
            key: key.peel_loc(db),
            value: value.peel_loc(db),
        })
        .expect("mappings are nameable")
    }

    pub fn fn_ptr(db: &dyn HirDb, sig: Fn<Type>) -> Type {
        Type::new(db, TyKind::FnPtr(sig)).expect("function pointers are nameable")
    }

    pub fn ty(self) -> Ty {
        self.0
    }

    pub fn is_invalid(self, db: &dyn HirDb) -> bool {
        self.0.is_invalid(db)
    }

    pub fn def_id(self, db: &dyn HirDb) -> Option<DefId> {
        self.0.def_id(db)
    }

    pub fn kind(self, db: &dyn HirDb) -> TyKind {
        self.0.kind(db)
    }

    pub fn peel_loc(self, db: &dyn HirDb) -> Type {
        Type(self.0.peel_loc(db))
    }

    pub fn loc(self, db: &dyn HirDb) -> Option<Location> {
        self.0.loc(db)
    }

    pub fn default_loc(self, db: &dyn HirDb) -> Option<Location> {
        match self.kind(db) {
            TyKind::Ref(inner, _) => inner.default_loc(db).or(Some(Location::Memory)),
            kind => kind.default_loc(),
        }
    }

    pub fn is_dynamically_encoded(self, db: &dyn HirDb) -> bool {
        self.0.is_dynamically_encoded(db)
    }

    /// Attach a location to a type, creating a reference if needed.
    /// An existing reference location is replaced — the only place a `Ref` is
    /// constructed, so a type can never carry two.
    pub fn at_loc(self, db: &dyn HirDb, loc: Location) -> Type {
        match self.kind(db) {
            TyKind::Ref(inner, _) => {
                Type::new(db, TyKind::Ref(inner, loc)).expect("Ref is nameable")
            }
            kind if kind.is_reference_type() => {
                Type::new(db, TyKind::Ref(self, loc)).expect("Ref is nameable")
            }
            _ => self,
        }
    }

    /// Attaches a location to a type, creating a reference if needed.
    /// An existing reference location remains the same.
    pub fn upcast(self, db: &dyn HirDb) -> Type {
        match self.kind(db) {
            TyKind::Ref(..) => self,
            kind => match kind.default_loc() {
                Some(loc) => self.at_loc(db, loc),
                None => self,
            },
        }
    }

    /// [`Self::at_loc`]
    pub fn upcast_from(self, db: &dyn HirDb, loc: Location) -> Type {
        self.at_loc(db, loc.as_reference())
    }

    pub fn implicitly_converts_to(
        self,
        target: impl Into<Ty>,
        db: &dyn HirDb,
    ) -> Option<u8> {
        self.0.implicitly_converts_to(target, db)
    }

    pub fn display(self, db: &dyn HirDb) -> String {
        self.0.display(db)
    }
}

