use la_arena::Idx;
use num_bigint::{BigInt, Sign};
use smol_str::SmolStr;
use tree_sitter::{Node, TreeCursor};

use crate::ast::kinds::{FieldKind, NodeKind};
use crate::ast::{AstNode, NodeRange};
use crate::hir::types::{LiteralKind, Location, Primitive, Ty, TyKind};
use crate::salsa::HirDb;

pub type Name = SmolStr;
pub type ExprId = Idx<Expr>;
pub type NamedValues = Box<[(Name, ExprId)]>;

#[derive(Clone, PartialEq, Eq)]
pub enum Expr {
    Ident(Name),
    Path(Box<str>),
    Literal(Literal),
    Binary {
        op: BinaryOp,
        left: ExprId,
        right: ExprId,
    },
    Unary {
        op: UnaryOp,
        arg: ExprId,
    },
    Update {
        op: UpdateOp,
        arg: ExprId,
    },
    Ternary {
        cond: ExprId,
        then: ExprId,
        els: ExprId,
    },
    /// `lhs = rhs` (`op: None`) and `lhs op= rhs` (`op: Some`).
    Assign {
        lhs: ExprId,
        rhs: ExprId,
        op: Option<BinaryOp>,
    },
    Member {
        obj: ExprId,
        prop: Name,
    },
    Call {
        callee: ExprId,
        args: Box<[ExprId]>,
    },
    /// `callee{name: value, ..}` — function call options.
    CallOptions {
        callee: ExprId,
        options: NamedValues,
    },
    /// Array and mapping access — `a[i]`, `m[k]`. `index` is absent for the type-syntax form `uint[]`.
    ArrayAccess {
        base: ExprId,
        index: Option<ExprId>,
    },
    /// `a[from:to]` — index-range access; `a[from:]`/`a[:to]`/`a[:]` are all legal.
    Slice {
        base: ExprId,
        from: Option<ExprId>,
        to: Option<ExprId>,
    },
    /// `[a, b, ..]` — an inline array literal: a fresh fixed-size `memory`
    /// array typed by the elements' common type, independent of context.
    InlineArray(Box<[ExprId]>),
    /// `(a, b, ..)` — a tuple expression, `None` is an omitted component (a hole), so
    /// `(a, , b)` has three components and `(x,)` two.
    Tuple(Box<[Option<ExprId>]>),
    MetaType(ExprId),
    New {
        ty: ExprId,
    },
    /// `{a: 1, b: 2}` — the braced named-argument group of a call/struct initialization.
    NamedArgs(NamedValues),
    /// Wrapper exprs. they exist to carry the context of the underlying expr
    ModifierInv(ExprId),
    Emit(ExprId),
    Revert(ExprId),
    Return(Option<ExprId>),
}


/// The *syntactic* shape of a `type_name` node. Classified by
/// [`ExprBuilder::type_shape`], which lives with the lowering because it is a
/// property of the syntax rather than of the type.
pub enum TypeShape {
    Function,
    Mapping,
    Array,
    Basic,
}

pub trait ExprBuilder {
    fn root(&self) -> &AstNode;
    #[inline]
    fn text<'n, 's: 'n>(&'s self, node: &'n Node<'n>) -> &'n str {
        self.root().text_by_range(node.byte_range()).trim()
    }
    fn alloc_expr_range(&mut self, expr: Expr, range: NodeRange) -> ExprId;
    /// `prop` is the property segment's range: it maps to the member so hover
    /// and definition on the name resolve to the member, while `range` is the
    /// member's own span.
    fn alloc_member_expr(&mut self, expr: Expr, range: NodeRange, prop: NodeRange) -> ExprId;
    fn alloc_call_expr(&mut self, call: Expr, node: Node, ident: Option<NodeRange>) -> ExprId;
    fn alloc_wrapper_expr(&mut self, expr: Expr, node: Node) -> ExprId;

    fn alloc_expr(&mut self, expr: Expr, node: Node) -> ExprId {
        self.alloc_expr_range(expr, NodeRange::from(&node))
    }

    fn type_shape(&self, node: Node) -> TypeShape {
        let s = self.text(&node).trim();
        if s.ends_with(']') {
            return TypeShape::Array;
        }
        let prefix = s.split(['(', ' ']).next().unwrap_or_default().trim();
        if prefix == "function" {
            return TypeShape::Function;
        }
        if prefix == "mapping" {
            return TypeShape::Mapping;
        }
        TypeShape::Basic
    }

    fn call_identifier(&self, node: Node) -> Option<NodeRange> {
        match node.kind_id().into() {
            NodeKind::IDENTIFIER => Some(NodeRange::from(&node)),
            NodeKind::TYPE => Some(NodeRange::from(&node)),
            NodeKind::PRIMITIVE_TYPE => Some(NodeRange::from(&node)),
            NodeKind::MEMBER_EXPRESSION => node
                .child_by_field_id(FieldKind::PROPERTY.into())
                .map(|prop| NodeRange::from(&prop)),
            NodeKind::EXPRESSION => self.call_identifier(named_expr_child(node, 0)?),
            _ => None,
        }
    }

    /// Ts grammar's hidden, flat `_identifier_path` yields sibling `identifier` nodes rather than a `member_expression`, 
    /// so a qualified path such as `Base.m` (`modifier_invocation`) or `L.f`
    /// (`user_defined_type`) arrives as two identifiers. 
    /// Folds them into the member chain, taking only the first would silently drop every later segment.
    fn lower_identifier_path(&mut self, node: Node) -> Option<(ExprId, NodeRange)> {
        let idents: Vec<Node> = node
            .named_children(&mut node.walk())
            .filter(|child| child.kind_id() == NodeKind::IDENTIFIER)
            .collect();
        let first = idents.first()?;
        let mut expr = self.alloc_expr(Expr::Ident(self.text(first).into()), *first);
        let start = first.start_byte() as u32;
        for prop in idents.iter().skip(1) {
            let name = self.text(prop).into();
            expr = self.alloc_member_expr(
                Expr::Member {
                    obj: expr,
                    prop: name,
                },
                NodeRange {
                    start,
                    end: prop.end_byte() as u32,
                },
                NodeRange::from(prop),
            );
        }
        Some((expr, NodeRange::from(idents.last()?)))
    }

    fn lower_named_values(&mut self, node: Node) -> NamedValues {
        node.named_children(&mut node.walk())
            .filter(|child| {
                matches!(
                    child.kind_id().into(),
                    NodeKind::CALL_STRUCT_ARGUMENT | NodeKind::NAMED_ARGUMENT
                )
            })
            .filter_map(|child| {
                let name = child.child_by_field_id(FieldKind::NAME.into())?;
                let value = child
                    .child_by_field_id(FieldKind::VALUE.into())
                    .filter(|value| !value.is_missing() && !value.has_error())
                    .and_then(|value| self.lower_expr(value))?;
                Some((self.text(&name).into(), value))
            })
            .collect()
    }

    /// Children are lowered (and so allocated) before their parent:
    /// `inference` sweeps the arena in reverse so a parent
    /// establishes the ambient context before its children are visited.
    fn lower_expr(&mut self, node: Node) -> Option<ExprId> {
        match node.kind_id().into() {
            NodeKind::NUMBER_LITERAL => {
                let text = self.text(&node);
                Some(self.alloc_expr(Expr::Literal(Literal::Number(text.into())), node))
            }
            NodeKind::HEX_STRING_LITERAL => {
                let text = self.text(&node);
                Some(self.alloc_expr(Expr::Literal(Literal::HexString(text.into())), node))
            }
            NodeKind::BOOLEAN_LITERAL => {
                let text = self.text(&node);
                Some(self.alloc_expr(Expr::Literal(Literal::Boolean(text == "true")), node))
            }
            NodeKind::STRING_LITERAL => {
                let text = self.text(&node);
                Some(self.alloc_expr(Expr::Literal(Literal::String(text.into())), node))
            }
            //types(primitive/user defined) contained in expressions are lowered as identifiers.
            //This is mainly to support type casts & udvt's e.g address(this)
            NodeKind::IDENTIFIER | NodeKind::PRIMITIVE_TYPE => {
                let ident = self.text(&node).into();
                (ident != "_").then(|| self.alloc_expr(Expr::Ident(ident), node))
            }
            // A qualified path in expression position is the flat `_identifier_path` 
            NodeKind::USER_DEFINED_TYPE => self.lower_identifier_path(node).map(|(expr, _)| expr),
            NodeKind::PAYABLE_CONVERSION_EXPRESSION => {
                let args = node
                    .named_children(&mut node.walk())
                    .filter(|n| n.kind_id() == NodeKind::CALL_ARGUMENT)
                    .filter_map(|n| self.lower_expr(n))
                    .collect::<Box<_>>();
                let keyword = node.child(0)?;
                let name = self.text(&keyword).into();
                let callee = self.alloc_expr(Expr::Ident(name), keyword);
                Some(self.alloc_call_expr(Expr::Call { callee, args }, node, None))
            }
            NodeKind::META_TYPE_EXPRESSION => {
                let type_node = node.child(0); //type is an unnamed node
                let ty = named_expr_child(node, 0).and_then(|c| self.lower_expr(c));

                let ident = type_node.and_then(|type_node| self.call_identifier(type_node));
                let expr = Expr::MetaType(ty?);
                // still alloc as call because of shape
                Some(self.alloc_call_expr(expr, node, ident))
            }
            NodeKind::NEW_EXPRESSION => {
                let ty = node.child_by_field_id(FieldKind::NAME.into()).and_then(|name| self.lower_expr(name));
                Some(self.alloc_expr(Expr::New { ty: ty? }, node))
            }
            NodeKind::INCOMPLETE_MEMBER_EXPRESSION => node
                .child_by_field_id(FieldKind::OBJECT.into())
                .and_then(|obj| self.lower_expr(obj)),
            NodeKind::MEMBER_EXPRESSION => {
                let obj = node
                    .child_by_field_id(FieldKind::OBJECT.into())
                    .and_then(|obj| self.lower_expr(obj));
                let prop = node.child_by_field_id(FieldKind::PROPERTY.into());
                let (name, prop) = prop
                    .map(|prop| {
                        (self.text(&prop).into(), NodeRange::from(&prop))
                    })
                    .unzip();

                Some(self.alloc_member_expr(
                    Expr::Member {
                        obj: obj?,
                        prop: name?,
                    },
                    NodeRange::from(&node),
                    prop?,
                ))
            }
            NodeKind::TYPE_NAME if matches!(self.type_shape(node), TypeShape::Array) => {
                let mut base = None;
                let mut size = None;
                for child in node.named_children(&mut node.walk()) {
                    match child.kind_id().into() {
                        NodeKind::TYPE_NAME => base = self.lower_expr(child),
                        NodeKind::EXPRESSION => size = self.lower_expr(child),
                        _ => {}
                    }
                }
                Some(self.alloc_expr(
                    Expr::ArrayAccess {
                        base: base?,
                        index: size,
                    },
                    node,
                ))
            }
            NodeKind::CALL_ARGUMENT => {
                let braced = node.child(0).is_some_and(|first| self.text(&first) == "{");
                if !braced {
                    return named_expr_child(node, 0).and_then(|arg| self.lower_expr(arg));
                }
                let named = self.lower_named_values(node);
                Some(self.alloc_expr(Expr::NamedArgs(named), node))
            }
            NodeKind::CALL_EXPRESSION => {
                let callee_node = node.child_by_field_id(FieldKind::FUNCTION.into());
                let callee = callee_node.and_then(|callee| self.lower_expr(callee));

                let ident = callee_node.and_then(|callee| self.call_identifier(callee));
                let args = node
                    .named_children(&mut node.walk())
                    .filter_map(|n| {
                        if n.kind_id() == NodeKind::CALL_ARGUMENT {
                            self.lower_expr(n)
                        } else {
                            None
                        }
                    })
                    .collect::<Box<_>>();

                let call_expr = Expr::Call {
                    callee: callee?,
                    args,
                };

                Some(self.alloc_call_expr(call_expr, node, ident))
            }
            NodeKind::BINARY_EXPRESSION => {
                let mut cursor = node.walk();
                let mut operands = named_expr_children(node, &mut cursor);
                let left = operands.next().and_then(|n| self.lower_expr(n));
                let right = operands.next().and_then(|n| self.lower_expr(n));
                let op = node
                    .child_by_field_id(FieldKind::OPERATOR.into())
                    .and_then(|op| BinaryOp::parse(self.text(&op)))?;
                Some(self.alloc_expr(
                    Expr::Binary {
                        op,
                        left: left?,
                        right: right?,
                    },
                    node,
                )) //late ? so both sides can fully lower.
            }
            NodeKind::UNARY_EXPRESSION => {
                // The operator is an anonymous token, so it is read from source;
                // the operand is the node's one named child.
                let arg = named_expr_child(node, 0).and_then(|arg| self.lower_expr(arg));
                let op = node.child(0).and_then(|operator| {
                    UnaryOp::parse(self.text(&operator))
                });
                Some(self.alloc_expr(
                    Expr::Unary {
                        op: op?,
                        arg: arg?,
                    },
                    node,
                ))
            }
            NodeKind::UPDATE_EXPRESSION => {
                // `x++` puts the operator after the operand and `++x` before it,
                // so the operand is the first named child and the operator the first unnamed.
                let arg = named_expr_child(node, 0).and_then(|arg| self.lower_expr(arg));
                let op = node
                    .children(&mut node.walk())
                    .find(|child| !child.is_named())
                    .and_then(|token| {
                        UpdateOp::parse(self.text(&token))
                    });
                Some(self.alloc_expr(
                    Expr::Update {
                        op: op?,
                        arg: arg?,
                    },
                    node,
                ))
            }
            NodeKind::TERNARY_EXPRESSION => {
                let mut cursor = node.walk();
                let mut operands = named_expr_children(node, &mut cursor);
                let cond = operands.next().and_then(|n| self.lower_expr(n));
                let then = operands.next().and_then(|n| self.lower_expr(n));
                let els = operands.next().and_then(|n| self.lower_expr(n));
                Some(self.alloc_expr(
                    Expr::Ternary {
                        cond: cond?,
                        then: then?,
                        els: els?,
                    },
                    node,
                ))
            }
            NodeKind::ASSIGNMENT_EXPRESSION => {
                let lhs = node
                    .child_by_field_id(FieldKind::LEFT.into())
                    .and_then(|lhs| self.lower_expr(lhs));
                let rhs = node
                    .child_by_field_id(FieldKind::RIGHT.into())
                    .and_then(|rhs| self.lower_expr(rhs));
                Some(self.alloc_expr(
                    Expr::Assign {
                        lhs: lhs?,
                        rhs: rhs?,
                        op: None,
                    },
                    node,
                ))
            }
            NodeKind::AUGMENTED_ASSIGNMENT_EXPRESSION => {
                let lhs = node
                    .child_by_field_id(FieldKind::LEFT.into())
                    .and_then(|lhs| self.lower_expr(lhs));
                let rhs = node
                    .child_by_field_id(FieldKind::RIGHT.into())
                    .and_then(|rhs| self.lower_expr(rhs));
                let op = node
                    .children(&mut node.walk())
                    .filter(|child| !child.is_named())
                    .find_map(|op| {
                        BinaryOp::parse(self.text(&op).strip_suffix('=')?)
                    });
                Some(self.alloc_expr(
                    Expr::Assign {
                        lhs: lhs?,
                        rhs: rhs?,
                        op: Some(op?),
                    },
                    node,
                ))
            }
            NodeKind::ARRAY_ACCESS => {
                let base = node
                    .child_by_field_id(FieldKind::BASE.into())
                    .and_then(|base| self.lower_expr(base));
                let index = node
                    .child_by_field_id(FieldKind::INDEX.into())
                    .and_then(|index| self.lower_expr(index));
                Some(self.alloc_expr(
                    Expr::ArrayAccess {
                        base: base?,
                        index,
                    },
                    node,
                ))
            }
            NodeKind::SLICE_ACCESS => {
                let base = node
                    .child_by_field_id(FieldKind::BASE.into())
                    .and_then(|base| self.lower_expr(base));
                let from = node
                    .child_by_field_id(FieldKind::FROM.into())
                    .and_then(|from| self.lower_expr(from));
                let to = node
                    .child_by_field_id(FieldKind::TO.into())
                    .and_then(|to| self.lower_expr(to));
                Some(self.alloc_expr(
                    Expr::Slice {
                        base: base?,
                        from,
                        to,
                    },
                    node,
                ))
            }
            NodeKind::INLINE_ARRAY_EXPRESSION => {
                let mut cursor = node.walk();
                let elems = named_expr_children(node, &mut cursor)
                    .map(|elem| self.lower_expr(elem))
                    .collect::<Option<Box<_>>>()?;
                Some(self.alloc_expr(Expr::InlineArray(elems), node))
            }
            NodeKind::CALL_OPTIONS => {
                let callee = node
                    .child_by_field_id(FieldKind::FUNCTION.into())
                    .and_then(|callee| self.lower_expr(callee));
                let options = self.lower_named_values(node);
                Some(self.alloc_expr(
                    Expr::CallOptions {
                        callee: callee?,
                        options,
                    },
                    node,
                ))
            }
            NodeKind::EMIT_STATEMENT => {
                //Emit statements are structured like expressions but not wrapped in an expression 🤷‍♂️
                let callee_node = node.child_by_field_id(FieldKind::NAME.into());
                let callee = callee_node.and_then(|callee| self.lower_expr(callee));

                let ident = callee_node.and_then(|callee| self.call_identifier(callee));
                let args = node
                    .named_children(&mut node.walk())
                    .filter_map(|n| {
                        if n.kind_id() == NodeKind::CALL_ARGUMENT {
                            self.lower_expr(n)
                        } else {
                            None
                        }
                    })
                    .collect::<Box<_>>();

                let call = self.alloc_call_expr(
                    Expr::Call {
                        callee: callee?,
                        args,
                    },
                    node,
                    ident,
                );
                Some(self.alloc_wrapper_expr(Expr::Emit(call), node))
            }
            NodeKind::REVERT_STATEMENT => {
                let callee_node = named_expr_child(node, 0);
                let callee = callee_node.and_then(|callee| self.lower_expr(callee));

                let args_node = node
                    .named_children(&mut node.walk())
                    .find(|n| n.kind_id() == NodeKind::REVERT_ARGUMENTS);

                let ident = callee_node.and_then(|callee| self.call_identifier(callee));
                let args = args_node
                    .map(|args_node| {
                        args_node
                            .named_children(&mut args_node.walk())
                            .filter_map(|n| {
                                if n.kind_id() == NodeKind::CALL_ARGUMENT {
                                    self.lower_expr(n)
                                } else {
                                    None
                                }
                            })
                            .collect::<Box<_>>()
                    })
                    .unwrap_or_default();

                let call = self.alloc_call_expr(
                    Expr::Call {
                        callee: callee?,
                        args,
                    },
                    node,
                    ident,
                );
                Some(self.alloc_wrapper_expr(Expr::Revert(call), node))
            }
            NodeKind::RETURN_STATEMENT => {
                let value = named_expr_child(node, 0)
                    .and_then(|value| self.lower_expr(value));
                Some(self.alloc_expr(Expr::Return(value), node))
            }
            NodeKind::INHERITANCE_SPECIFIER => {
                let (callee, name) = node
                    .named_children(&mut node.walk())
                    .find(|n| n.kind_id() == NodeKind::USER_DEFINED_TYPE)
                    .and_then(|base| self.lower_identifier_path(base))?;
                if !node
                    .children(&mut node.walk())
                    .any(|child| child.kind_id() == NodeKind::LPAREN)
                {
                    return Some(callee);
                }
                let args = node
                    .named_children(&mut node.walk())
                    .filter_map(|n| {
                        if n.kind_id() == NodeKind::CALL_ARGUMENT {
                            self.lower_expr(n)
                        } else {
                            None
                        }
                    })
                    .collect::<Box<_>>();
                let call = self.alloc_call_expr(Expr::Call { callee, args }, node, Some(name));
                Some(self.alloc_wrapper_expr(Expr::ModifierInv(call), node))
            }
            NodeKind::MODIFIER_INVOCATION | NodeKind::TYPECAST_EXPRESSION => {
                // A modifier invocation's callee is _identifier_path see [`Self::lower_identifier_path`], 
                // i.e. `Base.m()` and must be folded first into the member chain. 
                // A cast's callee is a `primitive_type` node.
                let callee = if node.kind_id() == NodeKind::MODIFIER_INVOCATION {
                    self.lower_identifier_path(node)
                        .map(|(callee, name)| (callee, Some(name)))
                } else {
                    named_expr_child(node, 0).and_then(|name_node| {
                        self.lower_expr(name_node)
                            .map(|callee| (callee, self.call_identifier(name_node)))
                    })
                };

                let args = node
                    .named_children(&mut node.walk())
                    .filter_map(|n| {
                        if n.kind_id() == NodeKind::CALL_ARGUMENT {
                            self.lower_expr(n)
                        } else {
                            None
                        }
                    })
                    .collect::<Box<_>>();

                let (callee, name) = callee?;
                let call = self.alloc_call_expr(Expr::Call { callee, args }, node, name);
                // Only a modifier invocation is a wrapper, so the invocation's
                // purpose survives lowering; a cast is just the call.
                if node.kind_id() == NodeKind::MODIFIER_INVOCATION {
                    Some(self.alloc_wrapper_expr(Expr::ModifierInv(call), node))
                } else {
                    Some(call)
                }
            }
            NodeKind::TUPLE_EXPRESSION => {
                // `(a, , b)` is `"(" a "," "," b ")"`: a hole is an *anonymous
                // comma*, so the components come from splitting the children on
                // commas — `named_children()` would silently drop the holes and
                // shift every later component.
                let mut elems: Vec<Option<ExprId>> = Vec::new();
                let mut pending: Option<ExprId> = None;
                let mut saw_component = false;
                for child in node.children(&mut node.walk()) {
                    if child.kind_id() == NodeKind::COMMENT {
                        continue;
                    }
                    if child.is_named() {
                        pending = self.lower_expr(child);
                        saw_component = true;
                    } else if self.text(&child) == "," {
                        elems.push(pending.take());
                        saw_component = true;
                    }
                }
                if saw_component {
                    elems.push(pending.take());
                }
                Some(self.alloc_expr(Expr::Tuple(elems.into_boxed_slice()), node))
            }
            _ => {
                //Bubble up intermediate exprs e.g.
                /*
                expression: "x"
                    {type_of}_expression: "x"
                 */

                // if single child we bubble the id up
                let children: Box<[_]> = named_expr_children(node, &mut node.walk()).collect();
                match children.as_ref() {
                    [only] => self.lower_expr(*only),
                    _ => {
                        for child in children {
                            self.lower_expr(child);
                        }
                        None
                    }
                }
            }
        }
    }
}

/// Named children minus `comment`. 
fn named_expr_children<'tree, 'cursor>( node: Node<'tree>, cursor: &'cursor mut TreeCursor<'tree>, ) -> impl Iterator<Item = Node<'tree>> + 'cursor {
    node.named_children(cursor).filter(|child| child.kind_id() != NodeKind::COMMENT)
}

/// The nth named child, skipping comments — see [`named_expr_children`].
fn named_expr_child<'tree>(node: Node<'tree>, index: usize) -> Option<Node<'tree>> {
    named_expr_children(node, &mut node.walk()).nth(index)
}


#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    And,
    Or,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

impl BinaryOp {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mul => "*",
            Self::Div => "/",
            Self::Mod => "%",
            Self::Pow => "**",
            Self::BitAnd => "&",
            Self::BitOr => "|",
            Self::BitXor => "^",
            Self::Shl => "<<",
            Self::Shr => ">>",
            Self::And => "&&",
            Self::Or => "||",
            Self::Eq => "==",
            Self::Ne => "!=",
            Self::Lt => "<",
            Self::Gt => ">",
            Self::Le => "<=",
            Self::Ge => ">=",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "+" => Self::Add,
            "-" => Self::Sub,
            "*" => Self::Mul,
            "/" => Self::Div,
            "%" => Self::Mod,
            "**" => Self::Pow,
            "&" => Self::BitAnd,
            "|" => Self::BitOr,
            "^" => Self::BitXor,
            "<<" => Self::Shl,
            ">>" => Self::Shr,
            "&&" => Self::And,
            "||" => Self::Or,
            "==" => Self::Eq,
            "!=" => Self::Ne,
            "<" => Self::Lt,
            ">" => Self::Gt,
            "<=" => Self::Le,
            ">=" => Self::Ge,
            _ => return None,
        })
    }
}

impl std::fmt::Display for BinaryOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// The one-operand operators. `Delete` is the only one that writes: it resets
/// its operand to the type's zero value, so its operand is an lvalue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    /// `-x`
    Neg,
    /// `!x`
    Not,
    /// `~x`
    BitNot,
    /// `delete x`
    Delete,
}

impl UnaryOp {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Neg => "-",
            Self::Not => "!",
            Self::BitNot => "~",
            Self::Delete => "delete",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "-" => Self::Neg,
            "!" => Self::Not,
            "~" => Self::BitNot,
            "delete" => Self::Delete,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateOp {
    Increment,
    Decrement,
}

impl UpdateOp {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Increment => "++",
            Self::Decrement => "--",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "++" => Self::Increment,
            "--" => Self::Decrement,
            _ => return None,
        })
    }
}


#[derive(Clone, PartialEq, Eq)]
pub enum Literal {
    Boolean(bool),
    String(SmolStr),
    Number(SmolStr),
    HexString(SmolStr),
}

impl Literal {
    fn parse_rational_value(text: &str) -> Option<(BigInt, BigInt)> {
        let text = text.trim().replace('_', "");
        let (text, multiplier) = Self::number_unit(&text);
        if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X"))
            && let Some(value) = BigInt::parse_bytes(hex.as_bytes(), 16)
        {
            return Some((value * multiplier, BigInt::from(1u8)));
        }

        let (mantissa, exponent) = text
            .split_once(['e', 'E'])
            .map(|(mantissa, exponent)| (mantissa, exponent.parse::<i32>().unwrap_or(0)))
            .unwrap_or((text, 0));
        let (negative, mantissa) = mantissa
            .strip_prefix('-')
            .map(|value| (true, value))
            .or_else(|| mantissa.strip_prefix('+').map(|value| (false, value)))
            .unwrap_or((false, mantissa));
        let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
        let digits = format!("{whole}{fraction}");
        let mut numerator = BigInt::parse_bytes(digits.as_bytes(), 10).unwrap_or_default();
        if negative {
            numerator = -numerator;
        }
        numerator *= multiplier;

        let scale = fraction.len() as i32 - exponent;
        if scale <= 0 {
            for _ in 0..(-scale) {
                numerator *= 10;
            }
            Some((numerator, BigInt::from(1u8)))
        } else {
            let mut denominator = BigInt::from(1u8);
            for _ in 0..scale {
                denominator *= 10;
            }
            Some((numerator, denominator))
        }
    }

    pub fn rational_value(&self) -> Option<(BigInt, BigInt)> {
        match self {
            Literal::Number(text) => Self::parse_rational_value(text),
            _ => None,
        }
    }

    fn parse_number(text: &str) -> LiteralKind {
        let normalized = text.trim().replace('_', "");
        let exact_bytes = normalized
            .strip_prefix("0x")
            .or_else(|| normalized.strip_prefix("0X"))
            .filter(|hex| !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .and_then(|hex| {
                (hex.len() % 2 == 0)
                    .then(|| u8::try_from(hex.len() / 2).ok())
                    .flatten()
            });
        match Self::parse_rational_value(text) {
            Some((num, den)) if num.clone() % &den == BigInt::from(0u8) => {
                Self::int_literal(num / den, exact_bytes)
            }
            _ => LiteralKind::Rational,
        }
    }

    fn int_literal(value: BigInt, exact_bytes: Option<u8>) -> LiteralKind {
        let neg = value.sign() == Sign::Minus;
        let magnitude = value.magnitude().clone();
        let bits = if neg { magnitude - 1u8 } else { magnitude }.bits();
        LiteralKind::Int {
            neg,
            min_bits: u16::try_from(bits).unwrap_or(u16::MAX),
            exact_bytes,
        }
    }

    /// `-l` / `~l` fold the operator into the literal's kind: solc types `-1`
    /// as `int_const -1`, not `int_const 1` — the sign is what makes
    /// `uint8 x = -1` reject. `None` for non-number literals and non-integral
    /// `~`.
    pub fn unary_kind(&self, op: UnaryOp) -> Option<LiteralKind> {
        let (num, den) = self.rational_value()?;
        let integral = den == BigInt::from(1u8);
        let value = match op {
            UnaryOp::Neg => -num,
            UnaryOp::BitNot if integral => -(num + 1u8),
            _ => return None,
        };
        Some(if integral {
            Self::int_literal(value, None)
        } else {
            LiteralKind::Rational
        })
    }

    /// The literal's produced bytes — escape-decoded for strings, hex pairs
    /// for `hex"…"`. `None` for non-byte literals and malformed escapes.
    ///
    /// Escape sequences count as the bytes they *produce*, not their source length
    /// "\x00"` is one byte, `"\u00e9"` two, and a backslash-newline is none.
    /// A string literal converts to `bytesN` only when it fits, so the
    /// fit test needs the decoded length, not `text.len()`.
    pub fn decoded_bytes(&self) -> Option<Vec<u8>> {
        match self {
            Literal::String(text) => decode_string(text),
            Literal::HexString(text) => decode_hex_string(text),
            _ => None,
        }
    }

    fn number_unit(text: &str) -> (&str, BigInt) {
        let units = [
            ("seconds", 1u64),
            ("minutes", 60),
            ("hours", 60 * 60),
            ("days", 60 * 60 * 24),
            ("weeks", 60 * 60 * 24 * 7),
            ("years", 60 * 60 * 24 * 365),
            ("gwei", 1_000_000_000),
            ("wei", 1),
            ("szabo", 1_000_000_000_000),
            ("finney", 1_000_000_000_000_000),
            ("ether", 1_000_000_000_000_000_000),
        ];
        units
            .iter()
            .find_map(|(unit, multiplier)| {
                text.strip_suffix(unit)
                    .map(|number| (number.trim_end(), BigInt::from(*multiplier)))
            })
            .unwrap_or((text, BigInt::from(1u8)))
    }

    pub fn source_text(&self) -> String {
        match self {
            Literal::Boolean(value) => value.to_string(),
            Literal::Number(value) => value.to_string(),
            Literal::String(value) => value.to_string(),
            Literal::HexString(value) => value.to_string(),
        }
    }

    pub fn integer_value(&self) -> Option<BigInt> {
        match self {
            Literal::Number(text) => Self::parse_rational_value(text)
                .and_then(|(num, den)| (num.clone() % &den == BigInt::from(0u8)).then(|| num / den)),
            _ => None,
        }
    }

    fn literal_kind(&self) -> LiteralKind {
        match self {
            Literal::Number(value) => Self::parse_number(value),
            Literal::String(_) => match self.decoded_bytes() {
                Some(bytes) => LiteralKind::String {
                    utf8: str::from_utf8(&bytes).is_ok(),
                    len: bytes.len() as u32,
                },
                // A malformed escape fits no `bytesN` — `u32::MAX` converts
                // nowhere, matching solc's rejection of the literal itself.
                None => LiteralKind::String {
                    utf8: false,
                    len: u32::MAX,
                },
            },
            Literal::HexString(_) => match self.decoded_bytes() {
                Some(bytes) => LiteralKind::Hex {
                    len: bytes.len() as u32,
                },
                None => LiteralKind::Hex { len: u32::MAX },
            },
            Literal::Boolean(_) => unreachable!("boolean literals lower to Primitive::Boolean"),
        }
    }

    pub fn loc(&self) -> Location {
        match self {
            Literal::String(_) | Literal::HexString(_) => Location::Memory,
            _ => Location::Stack,
        }
    }

    pub fn ty(&self, db: &dyn HirDb) -> Ty {
        Ty::new(
            db,
            match self {
                Literal::Boolean(_) => TyKind::Primitive(Primitive::Boolean),
                _ => TyKind::Literal(self.literal_kind()),
            },
        )
    }
}

// ── Literal decoding ─────────────────────────────────────────────────────

/// Decodes a string literal's source text to its produced bytes — `\xNN`
/// and `\uNNNN` escapes, `\n`/`\r`/`\t`, escaped quotes/backslash, and
/// backslash-newline continuations. `None` on a malformed escape.
fn decode_string(text: &str) -> Option<Vec<u8>> {
    let content = text.trim().trim_matches(|c| c == '"' || c == '\'');
    let mut out = Vec::new();
    let mut chars = content.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.extend_from_slice(c.to_string().as_bytes());
            continue;
        }
        match chars.next()? {
            '\n' => {}
            'n' => out.push(b'\n'),
            'r' => out.push(b'\r'),
            't' => out.push(b'\t'),
            'x' => {
                let hi = chars.next()?.to_digit(16)?;
                let lo = chars.next()?.to_digit(16)?;
                out.push((hi * 16 + lo) as u8);
            }
            'u' => {
                let codepoint = (0..4).try_fold(0u32, |acc, _| {
                    chars.next()?.to_digit(16).map(|digit| acc * 16 + digit)
                })?;
                let c = char::from_u32(codepoint)?;
                out.extend_from_slice(c.to_string().as_bytes());
            }
            other => out.extend_from_slice(other.to_string().as_bytes()),
        }
    }
    Some(out)
}

/// Decodes `hex"…"` — the hex digit pairs, skipping `_` separators.
fn decode_hex_string(text: &str) -> Option<Vec<u8>> {
    let content = text
        .trim()
        .trim_start_matches("hex")
        .trim_matches(|c| c == '"' || c == '\'');
    let digits: Option<Vec<u8>> = content
        .chars()
        .filter(|c| *c != '_')
        .map(|c| c.to_digit(16).map(|digit| digit as u8))
        .collect();
    let digits = digits?;
    if digits.is_empty() || !digits.len().is_multiple_of(2) {
        return None;
    }
    Some(digits.chunks(2).map(|pair| pair[0] * 16 + pair[1]).collect())
}

#[cfg(test)]
mod tests {
    use super::Literal;
    use num_bigint::BigInt;

    #[test]
    fn numeric_units_are_scaled_before_type_inference() {
        assert_eq!(
            Literal::Number("1 ether".into()).integer_value(),
            Some(BigInt::from(1_000_000_000_000_000_000u64))
        );
        assert_eq!(
            Literal::Number("2 hours".into()).integer_value(),
            Some(BigInt::from(7_200u64))
        );
        assert_eq!(
            Literal::Number("3 gwei".into()).integer_value(),
            Some(BigInt::from(3_000_000_000u64))
        );
    }

    #[test]
    fn string_literal_escapes_decode_to_the_bytes_they_produce() {
        let cases = [
            ("abc", 3),
            (r"\n", 1),
            (r"\x00\x00", 2),
            (r"\u00e9", 2),
            ("a\\\nb", 2),
        ];
        for (content, expected) in cases {
            let bytes = Literal::String(content.into())
                .decoded_bytes()
                .expect("{content:?} should decode");
            assert_eq!(
                bytes.len(),
                expected,
                "{content:?} should decode to {expected} bytes"
            );
            assert!(
                str::from_utf8(&bytes).is_ok(),
                "{content:?} should stay valid UTF-8"
            );
        }
        let high_byte = Literal::String(r"\x80".into())
            .decoded_bytes()
            .expect("a \\xNN escape should decode");
        assert!(str::from_utf8(&high_byte).is_err());
    }
}
