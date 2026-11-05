use la_arena::{Arena, Idx};
use tree_sitter::Node;

use crate::ast::kinds::{FieldKind, NodeKind};
use crate::hir::exprs::{ExprBuilder, ExprId, Name, TypeShape};
use crate::hir::types::func::Fn;
use crate::hir::types::primitive::Primitive;

pub type TyNameId = Idx<TyName>;
#[derive(PartialEq, Eq)]
pub enum TyName {
    Primitive(Primitive),
    UserDefined { base: Option<TyNameId>, name: Name },
    Array { ty: TyNameId, size: Option<ExprId> },
    Mapping { key: TyNameId, value: TyNameId },
    Located { ty: TyNameId, loc: Location },
    Fn(Fn<TyNameId>), //fn ptr
}

impl TyName {
    pub fn to_string(&self, arena: &Arena<TyName>) -> String {
        match self {
            TyName::Primitive(p) => p.to_string(),
            TyName::UserDefined { base, name } => base
                .map(|base| format!("{}.{}", arena[base].to_string(arena), name.as_str()))
                .unwrap_or(name.to_string()),
            TyName::Array { ty, size } => {
                let base = arena[*ty].to_string(arena);
                match size {
                    Some(_) => format!("{}[_]", base),
                    None => format!("{}[]", base),
                }
            }
            TyName::Mapping { key, value } => {
                format!(
                    "mapping({} => {})",
                    arena[*key].to_string(arena),
                    arena[*value].to_string(arena)
                )
            }
            TyName::Fn(f) => {
                let params = f
                    .params
                    .iter()
                    .map(|&p| arena[p].to_string(arena))
                    .collect::<Vec<_>>()
                    .join(", ");
                let ret = f
                    .returns
                    .iter()
                    .map(|&r| arena[r].to_string(arena))
                    .collect::<Vec<_>>()
                    .join(", ");
                let mut s = format!("function({}) {}", params, f.visibility.as_str());
                if f.mutability != Mutability::NonPayable {
                    s.push(' ');
                    s.push_str(f.mutability.as_str());
                }
                if !f.returns.is_empty() {
                    s.push_str(&format!(" returns ({})", ret));
                }
                s
            }
            TyName::Located { ty, loc } => {
                let base = arena[*ty].to_string(arena);
                let loc = match loc {
                    Location::Memory => " memory",
                    Location::Calldata => " calldata",
                    Location::Storage(_) => " storage",
                    Location::Stack => "",
                };
                format!("{base}{loc}")
            }
        }
    }
}

#[derive(Default, PartialEq, Eq, Clone, Copy, Hash)]
pub enum Visibility {
    #[default]
    Internal,
    Public,
    Private,
    External,
}

impl Visibility {
    #[inline]
    pub fn parse(s: &str) -> Visibility {
        match s {
            "public" => Visibility::Public,
            "private" => Visibility::Private,
            "external" => Visibility::External,
            _ => Visibility::Internal,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Visibility::Internal => "internal",
            Visibility::Public => "public",
            Visibility::Private => "private",
            Visibility::External => "external",
        }
    }
}

#[derive(Default, PartialEq, Eq, Clone, Copy, Hash, Debug)]
pub enum Mutability {
    #[default]
    NonPayable,
    Payable,
    View,
    Pure,
}

impl Mutability {
    #[inline]
    pub fn parse(s: &str) -> Mutability {
        match s {
            "payable" => Mutability::Payable,
            "view" => Mutability::View,
            "pure" => Mutability::Pure,
            _ => Mutability::NonPayable,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Mutability::NonPayable => "",
            Mutability::Payable => "payable",
            Mutability::View => "view",
            Mutability::Pure => "pure",
        }
    }

    pub fn implicitly_converts_to(self, target: Mutability) -> bool {
        self == target
            || matches!(
                (self, target),
                (Mutability::Pure, Mutability::View | Mutability::NonPayable)
                    | (Mutability::View, Mutability::NonPayable)
                    | (Mutability::Payable, Mutability::NonPayable)
            )
    }

    /// Projects `Payable` onto `NonPayable` for state-effect comparisons:
    /// payability is a separate axis (checked through `msg.value` and call
    /// options), so a payable body is only as strict as a non-payable one about
    /// what it reads and writes.
    pub fn state_effect(self) -> Self {
        match self {
            Self::Payable => Self::NonPayable,
            other => other,
        }
    }

    /// Least upper bound on the state-effect axis (`Pure < View < NonPayable`):
    /// a body needing both a read and a write needs the stricter of the two.
    pub fn join(self, other: Self) -> Self {
        match (self.state_effect(), other.state_effect()) {
            (Self::NonPayable, _) | (_, Self::NonPayable) => Self::NonPayable,
            (Self::View, _) | (_, Self::View) => Self::View,
            _ => Self::Pure,
        }
    }
}


#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum VariableKind {
    #[default]
    LocalVariable,
    Parameter,
    Const,
    Immutable,
    StateVariable,
}

impl std::fmt::Display for VariableKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VariableKind::Parameter => write!(f, "Parameter"),
            VariableKind::LocalVariable => write!(f, "Local Variable"),
            VariableKind::Const => write!(f, "Constant"),
            VariableKind::Immutable => write!(f, "Immutable"),
            VariableKind::StateVariable => write!(f, "State Variable"),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefKind {
    #[default]
    /// A bare storage refernce
    Reference,
    /// A pointer to a storage reference (specifically for local declared storage variables)
    Pointer,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Location {
    #[default]
    ///alias for NONE (might rename/remove) for builtins/primitive types
    Stack,
    Memory,
    Calldata,
    /// Storage distinguishes a bound reference from a pointer.
    Storage(RefKind),
}

impl Location {
    /// Whether a value stored in `from` may be implicitly converted to a target
    /// in `self`. A storage *pointer* only accepts a storage source; a bound
    /// storage reference also accepts `memory`/`calldata`, which are copied in;
    /// there is no implicit conversion *into* `calldata`.
    pub fn accepts(self, from: Location) -> bool {
        match self {
            Location::Calldata => matches!(from, Location::Calldata),
            Location::Storage(RefKind::Pointer) => from.is_storage(),
            _ => true,
        }
    }

    pub fn is_storage(self) -> bool {
        matches!(self, Location::Storage(_))
    }

    /// The same location as a bound reference — derivations (`p.field`, `p[i]`,
    /// a mapping value) never rebind, they name a slot.
    pub fn as_reference(self) -> Location {
        match self {
            Location::Storage(_) => Location::Storage(RefKind::Reference),
            other => other,
        }
    }
}

impl std::fmt::Display for Location {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Location::Memory => write!(f, "memory"),
            Location::Calldata => write!(f, "calldata"),
            Location::Storage(RefKind::Reference) => write!(f, "storage"),
            Location::Storage(RefKind::Pointer) => write!(f, "storage pointer"),
            Location::Stack => write!(f, ""),
        }
    }
}
pub trait TypeBuilder: ExprBuilder {
    fn alloc_type(&mut self, ty: TyName, node: Node) -> TyNameId;

    fn lower_type(&mut self, node: Node) -> Option<TyNameId> {
        match node.kind_id().into() {
            NodeKind::PRIMITIVE_TYPE => {
                let type_str = self.root().text_by_range(node.byte_range()).trim();
                let ty = Primitive::parse(type_str);
                Some(self.alloc_type(TyName::Primitive(ty), node))
            }
            NodeKind::USER_DEFINED_TYPE => {
                let mut base = None;
                for ident in node.named_children(&mut node.walk()) {
                    if ident.kind_id() == NodeKind::IDENTIFIER {
                        let name: Name =
                            self.root().text_by_range(ident.byte_range()).trim().into();
                        base = Some(self.alloc_type(TyName::UserDefined { base, name }, ident));
                    }
                }
                base
            }
            NodeKind::TYPE_NAME => {
                match self.type_shape(node) {
                    TypeShape::Function => Some(self.lower_fn_type(node)),
                    TypeShape::Mapping => {
                        let key = node
                            .child_by_field_id(FieldKind::KEY_TYPE.into())
                            .and_then(|k| self.lower_type(k));

                        let value = node
                            .child_by_field_id(FieldKind::VALUE_TYPE.into())
                            .and_then(|v| self.lower_type(v));

                        if let (Some(key), Some(value)) = (key, value) {
                            return Some(self.alloc_type(TyName::Mapping { key, value }, node));
                        }
                        None
                    }
                    TypeShape::Array => {
                        let mut base = None;
                        let mut size = None;

                        for child in node.named_children(&mut node.walk()) {
                            match child.kind_id().into() {
                                NodeKind::TYPE_NAME => {
                                    base = self.lower_type(child);
                                }
                                NodeKind::EXPRESSION => {
                                    size = self.lower_expr(child);
                                }
                                _ => {}
                            }
                        }

                        if let Some(ty) = base {
                            return Some(self.alloc_type(TyName::Array { ty, size }, node));
                        }
                        None
                    }
                    TypeShape::Basic => {
                        for child in node.named_children(&mut node.walk()) {
                            //Fix: we return on the first match. can there be more??
                            if matches!(
                                child.kind_id().into(),
                                NodeKind::PRIMITIVE_TYPE
                                    | NodeKind::USER_DEFINED_TYPE
                                    | NodeKind::TYPE_NAME
                            ) {
                                return self.lower_type(child);
                            }
                        }
                        None
                    }
                }
            }
            _ => None,
        }
    }

    fn lower_fn_type(&mut self, node: Node) -> TyNameId {
        let mut params = Vec::new();
        let mut ret = Vec::new();
        let mut fn_ty = Fn::<TyNameId>::default();

        for child in node.named_children(&mut node.walk()) {
            match child.kind_id().into() {
                kind @ (NodeKind::PARAMETER | NodeKind::RETURN_PARAMETER) => {
                    let mut location = None;
                    for part in child.children(&mut child.walk()) {
                        match part.kind_id().into() {
                            NodeKind::MEMORY => location = Some(Location::Memory),
                            NodeKind::STORAGE => {
                                location = Some(Location::Storage(RefKind::Pointer))
                            }
                            NodeKind::CALLDATA => location = Some(Location::Calldata),
                            _ => {}
                        }
                    }
                    if let Some(param_ty) = child.child_by_field_id(FieldKind::TYPE.into())
                        && let Some(ty) = self.lower_type(param_ty)
                    {
                        let ty = match location {
                            Some(loc) => {
                                self.alloc_type(TyName::Located { ty, loc }, child)
                            }
                            None => ty,
                        };
                        if matches!(kind, NodeKind::PARAMETER) {
                            params.push(ty);
                        } else {
                            ret.push(ty);
                        }
                    }
                }
                NodeKind::VISIBILITY => {
                    //I Think public/private are not allowed for fn poiinters
                    let vis_text = self.root().text_by_range(child.byte_range()).trim();
                    fn_ty.visibility = Visibility::parse(vis_text);
                }
                NodeKind::STATE_MUTABILITY => {
                    let mut_text = self.root().text_by_range(child.byte_range()).trim();
                    fn_ty.mutability = Mutability::parse(mut_text);
                }
                _ => {}
            }
        }

        fn_ty.params = params.into_boxed_slice();
        fn_ty.returns = ret.into_boxed_slice();

        self.alloc_type(TyName::Fn(fn_ty), node)
    }
}

