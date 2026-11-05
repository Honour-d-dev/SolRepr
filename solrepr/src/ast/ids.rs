use crate::ast::ast::{AstNode, ToAstNode};
use crate::ast::kinds::NodeKind;
use crate::ast::{self, ByteRange};
use crate::salsa::FileId;
use rustc_hash::FxHashMap;
use std::marker::PhantomData;
use std::{
    collections::VecDeque,
    hash::{Hash, Hasher},
};
use tree_sitter::{Node, Range};

/// file-local node index wrapper
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ErasedAstId {
    id: u32,
    parent: Option<u32>,
}

/// file-local index with type data
#[derive(Debug, PartialEq, Eq)]
pub struct AstId<N> {
    raw: ErasedAstId,
    _ty: PhantomData<fn() -> N>,
}

// manual impls to avoid N: clone/copy bound on derived version
impl<N> Clone for AstId<N> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<N> Copy for AstId<N> {}

impl<N> Hash for AstId<N> {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        self.raw.hash(hasher);
    }
}

impl<N> AstId<N> {
    pub fn erase(self) -> ErasedAstId {
        self.raw
    }
    pub fn from_erased(raw: ErasedAstId) -> Self {
        Self {
            raw,
            _ty: PhantomData,
        }
    }
    pub fn upcast<M: ToAstNode>(self) -> AstId<M> {
        AstId::from_erased(self.raw)
    }
}

pub fn is_supported_node(node: &Node<'_>) -> bool {
    matches!(
        NodeKind::from(node.kind_id()),
        NodeKind::IMPORT_DIRECTIVE
            | NodeKind::USING_DIRECTIVE
            | NodeKind::USER_DEFINED_TYPE_DEFINITION
            | NodeKind::CONTRACT_DEFINITION
            | NodeKind::CONTRACT_BODY
            | NodeKind::INTERFACE_DEFINITION
            | NodeKind::LIBRARY_DEFINITION
            | NodeKind::FUNCTION_DEFINITION
            | NodeKind::CONSTRUCTOR_DEFINITION
            | NodeKind::FALLBACK_RECEIVE_DEFINITION
            | NodeKind::STRUCT_DEFINITION
            | NodeKind::ENUM_DEFINITION
            | NodeKind::EVENT_DEFINITION
            | NodeKind::ERROR_DEFINITION
            | NodeKind::MODIFIER_DEFINITION
            | NodeKind::STATE_VAR_DECLARATION
            | NodeKind::CONST_VAR_DECLARATION
    )
}

#[derive(Debug, PartialEq, Eq)]
pub struct AstIdMap {
    ptr_to_id: FxHashMap<NodePtr, ErasedAstId>,
    id_to_ptr: FxHashMap<ErasedAstId, NodePtr>,
}

impl AstIdMap {
    pub fn new(root: &AstNode) -> Self {
        assert!(root.is_root());

        // Very shallow/simplified id allocation algo for now: does BFS
        // This id system only works for 2 layers, thankfully we don't need more than 2 layers
        // Global layer: consts, free fns, contracts/interface/library etc.
        // 2nd layer: contract/interface/library members
        // everything below function level is ignored
        let mut ptr_id = 0;
        let mut ptr_to_id = FxHashMap::default();
        let mut id_to_ptr = FxHashMap::default();
        let mut queue = VecDeque::new();
        let mut parent_map = FxHashMap::default();

        queue.push_back((root.node(), None));

        while let Some((node, parent)) = queue.pop_front() {
            let node_ptr = NodePtr::from_node(node);
            let id = Self::generate_id(parent, ptr_id, &mut parent_map);
            ptr_id += 1;
            ptr_to_id.insert(node_ptr, id);
            id_to_ptr.insert(id, node_ptr);

            for child in node.named_children(&mut node.walk())
                .filter(is_supported_node)
            {
                //source file should not be a parent to top level items
                let parent = if node.kind_id() == NodeKind::SOURCE_FILE {
                    None
                } else {
                    Some(id)
                };
                queue.push_back((child, parent));
            }
        }

        Self {
            ptr_to_id,
            id_to_ptr,
        }
        .shrink_to_fit()
    }

    fn shrink_to_fit(mut self) -> Self {
        self.ptr_to_id.shrink_to_fit();
        self.id_to_ptr.shrink_to_fit();
        self
    }

    fn generate_id(
        parent: Option<ErasedAstId>,
        index: usize,
        parent_map: &mut FxHashMap<usize, usize>,
    ) -> ErasedAstId {
        if let Some(parent) = parent {
            let idx = parent_map.entry(parent.id as usize).or_default();
            let id = *idx;
            *idx += 1;
            ErasedAstId {
                id: id as u32,
                parent: Some(parent.id),
            }
        } else {
            ErasedAstId {
                id: index as u32,
                parent: None,
            }
        }
    }

    fn erased_id(&self, ptr: NodePtr) -> Option<ErasedAstId> {
        self.ptr_to_id.get(&ptr).copied()
    }

    fn id_of_ptr<N: ToAstNode>(&self, ptr: NodePtr) -> Option<AstId<N>> {
        N::can_cast(ptr.kind)
            .then(|| self.erased_id(ptr).map(AstId::from_erased))
            .flatten()
    }

    pub fn id_of_node<N: ToAstNode>(&self, n: Node<'_>) -> Option<AstId<N>> {
        self.id_of_ptr(NodePtr::from_node(n))
    }

    pub fn id<N: ToAstNode>(&self, n: &N) -> Option<AstId<N>> {
        let ptr = NodePtr::from_ast_node(n.to_node_ref());
        self.id_of_ptr(ptr)
    }

    pub fn get<N: ToAstNode>(&self, root: &AstNode, id: AstId<N>) -> Option<N> {
        let ptr = self.id_to_ptr.get(&id.erase()).copied()?;
        //Assert root contains node?
        ptr.to_node(root).and_then(N::cast)
    }

    pub fn get_node(&self, root: &AstNode, id: ErasedAstId) -> Option<AstNode> {
        let ptr = self.id_to_ptr.get(&id).copied()?;
        ptr.to_node(root)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeRange {
    pub start: u32,
    pub end: u32,
}

impl NodeRange {
    #[inline]
    pub fn from(node: &Node) -> Self {
        Self {
            start: node.start_byte() as u32,
            end: node.end_byte() as u32,
        }
    }

    #[inline]
    pub fn from_range(range: Range) -> Self {
        Self {
            start: range.start_byte as u32,
            end: range.end_byte as u32,
        }
    }

    #[inline]
    pub fn from_byte_range(range: ByteRange) -> Self {
        Self {
            start: range.start as u32,
            end: range.end as u32,
        }
    }

    #[inline]
    pub fn contains(&self, other: &NodeRange) -> bool {
        self.start <= other.start && self.end >= other.end
    }
}

impl From<NodeRange> for ByteRange {
    fn from(val: NodeRange) -> Self {
        val.start as usize..val.end as usize
    }
}

/// A light-weight repr of a node with kind info
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodePtr {
    kind: NodeKind,
    range: NodeRange,
}

impl NodePtr {
    #[inline]
    pub fn from_ast_node(node: &AstNode) -> Self {
        Self {
            kind: node.node().kind_id().into(),
            range: NodeRange::from_range(node.node().range()),
        }
    }

    #[inline]
    pub fn from_node(node: Node) -> Self {
        Self {
            kind: node.kind_id().into(),
            range: NodeRange::from_range(node.range()),
        }
    }

    pub fn to_node(self, root: &AstNode) -> Option<AstNode> {
        root.child_node(self.range)
            .filter(|a| a.node().kind_id() == self.kind)
    }

    //Note: we can also go from ptr to typed ast::item
}

/// Workspace unique item ids

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ImportId {
    pub file: FileId,
    pub id: AstId<ast::Import>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UdvtId {
    pub file: FileId,
    pub id: AstId<ast::Udvt>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UsingId {
    pub file: FileId,
    pub id: AstId<ast::Using>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ContractId {
    pub file: FileId,
    pub id: AstId<ast::Contract>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct InterfaceId {
    pub file: FileId,
    pub id: AstId<ast::Interface>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LibraryId {
    pub file: FileId,
    pub id: AstId<ast::Library>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FunctionId {
    pub file: FileId,
    pub id: AstId<ast::Function>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ModifierId {
    pub file: FileId,
    pub id: AstId<ast::Modifier>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StructId {
    pub file: FileId,
    pub id: AstId<ast::Struct>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EventId {
    pub file: FileId,
    pub id: AstId<ast::Event>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EnumId {
    pub file: FileId,
    pub id: AstId<ast::Enum>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ErrorId {
    pub file: FileId,
    pub id: AstId<ast::Error>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VarId {
    pub file: FileId,
    pub id: AstId<ast::Var>,
}
