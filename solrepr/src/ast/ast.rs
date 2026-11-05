use crate::ast::{ids::NodeRange, kinds::NodeKind};
use std::rc::Rc;
use tree_sitter::{Node, Tree, ffi::TSNode};
use triomphe::Arc;

pub type ByteRange = std::ops::Range<usize>;

/// RA-like Ast structure but using tree-sitter and string views
#[derive(Debug, Clone)]
pub struct Ast {
    tree: Tree,
    source: Arc<str>,
}

impl Ast {
    pub fn new(tree: Tree, source: Arc<str>) -> Self {
        Self {
            tree, 
            source
        }
    }

    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn root(&self) -> AstNode {
        AstNode {
            node: self.tree().root_node().into_raw(),
            ast: Rc::new(self.clone()),
        }
    }
}

/// Manual impl to avoid doing a deep tree comparison (node by node)
/// if the sources are equal, the trees are as well
/// Allthough source comparison is less stable 
/// ie. slightly different source can produce identical trees as the parser does not account whitespaces
/// Its more predictable.
impl PartialEq for Ast {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
    }
}
impl Eq for Ast {}

type AstInner = Rc<Ast>;

/// RA-like RedNode/SyntaxNode structure
/// But Ts nodes dont own their strings, so instead we share a ref to the ast
/// This also means we can use the raw TsNode instead of Node<'a> and avoid lifetime bounds/coloring
/// as the tree is always in hand
#[derive(Debug, Clone)]
pub struct AstNode {
    node: TSNode,
    ast: AstInner,
}

impl PartialEq for AstNode {
    fn eq(&self, other: &Self) -> bool {
        self.node.id == other.node.id
    }
}

impl Eq for AstNode {}

impl AstNode {
    pub fn new(node: TSNode, ast: AstInner) -> Self {
        Self { node, ast }
    }

    pub fn node(&self) -> Node<'_> {
        unsafe { Node::from_raw(self.node) }
    }

    pub fn raw_node(&self) -> TSNode {
        self.node
    }

    pub fn ast(&self) -> Ast {
        (*self.ast).clone()
    }

    #[inline]
    pub fn text(&self) -> &str {
        &self.ast.source[self.node().byte_range()]
    }

    #[inline]
    pub fn text_by_range(&self, range: ByteRange) -> &str {
        &self.ast.source[range]
    }

    /// The top level node
    pub fn is_root(&self) -> bool {
        self.node().kind_id() == NodeKind::SOURCE_FILE
    }

    /// Converts a tree-sitter Node<'a> to an AstNode
    pub fn upcast(&self, node: Node<'_>) -> AstNode {
        AstNode::new(node.into_raw(), self.ast.clone())
    }

    pub fn child_node(&self, range: NodeRange) -> Option<AstNode> {
        self.node()
            .descendant_for_byte_range(range.start as usize, range.end as usize)
            .map(|node| AstNode::new(node.into_raw(), self.ast.clone()))
    }

    pub fn named_child_node(&self, range: NodeRange) -> Option<AstNode> {
        self.node()
            .named_descendant_for_byte_range(range.start as usize, range.end as usize)
            .map(|node| AstNode::new(node.into_raw(), self.ast.clone()))
    }

    pub fn children<'a>(&'a self, cursor: &'a mut tree_sitter::TreeCursor<'a>) -> impl Iterator<Item = AstNode> + 'a {
        assert!(self.node() == cursor.node());
        self.node()
            .named_children(cursor)
            .map(|node| AstNode::new(node.into_raw(), self.ast.clone()))
    }
}

/// Helper trait for ast types that convert to AstNodes
pub trait ToAstNode: Sized {
    fn to_node(self) -> AstNode;

    fn to_node_ref(&self) -> &AstNode;

    fn cast(n: AstNode) -> Option<Self>;

    fn can_cast(n: NodeKind) -> bool;
}
