#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct NodeKind(u16);

impl NodeKind {
    #[inline]
    pub const fn as_u16(self) -> u16 {
        self.0
    }
}

impl From<u16> for NodeKind {
    #[inline]
    fn from(id: u16) -> Self {
        Self(id)
    }
}

impl From<NodeKind> for u16 {
    #[inline]
    fn from(kind: NodeKind) -> Self {
        kind.0
    }
}

impl PartialEq<NodeKind> for u16 {
    #[inline]
    fn eq(&self, other: &NodeKind) -> bool {
        *self == other.0
    }
}

impl NodeKind {
    // Thin wrapper around TS node kind IDs,
    //          named nodes
    pub const IDENTIFIER: NodeKind = NodeKind(1);
    pub const COMMENT: NodeKind = NodeKind(328);
    pub const SOURCE_FILE: NodeKind = NodeKind(329);

    pub const USING_DIRECTIVE: NodeKind = NodeKind(369);
    pub const ENUM_DEFINITION: NodeKind = NodeKind(363);
    pub const EVENT_DEFINITION: NodeKind = NodeKind(365);
    pub const IMPORT_DIRECTIVE: NodeKind = NodeKind(339);
    pub const ERROR_DEFINITION: NodeKind = NodeKind(351);
    pub const STRUCT_DEFINITION: NodeKind = NodeKind(360);
    pub const LIBRARY_DEFINITION: NodeKind = NodeKind(354);
    pub const MODIFIER_DEFINITION: NodeKind = NodeKind(417);
    pub const CONSTRUCTOR_DEFINITION: NodeKind = NodeKind(418);
    pub const FALLBACK_RECEIVE_DEFINITION: NodeKind = NodeKind(419);
    pub const CONTRACT_DEFINITION: NodeKind = NodeKind(350);
    pub const FUNCTION_DEFINITION: NodeKind = NodeKind(420);
    pub const INTERFACE_DEFINITION: NodeKind = NodeKind(353);

    pub const ENUM_VALUE: NodeKind = NodeKind(530);
    pub const STRUCT_BODY: NodeKind = NodeKind(362);
    pub const CONTRACT_BODY: NodeKind = NodeKind(358);
    pub const FUNCTION_BODY: NodeKind = NodeKind(426);
    pub const STRUCT_MEMBER: NodeKind = NodeKind(361);
    pub const INHERITANCE_SPECIFIER: NodeKind = NodeKind(357);

    pub const PARAMETER: NodeKind = NodeKind(456);
    pub const ERROR_PARAMETER: NodeKind = NodeKind(352);
    pub const EVENT_PARAMETER: NodeKind = NodeKind(367);
    pub const VAR_DECLARATION: NodeKind = NodeKind(398);
    pub const STATE_VAR_DECLARATION: NodeKind = NodeKind(412);
    pub const VAR_DECLARATION_TUPLE: NodeKind = NodeKind(399);
    pub const CONST_VAR_DECLARATION: NodeKind = NodeKind(349);

    pub const STATEMENT: NodeKind = NodeKind(372);
    pub const IF_STATEMENT: NodeKind = NodeKind(401);
    pub const TRY_STATEMENT: NodeKind = NodeKind(408);
    pub const FOR_STATEMENT: NodeKind = NodeKind(402);
    pub const BLOCK_STATEMENT: NodeKind = NodeKind(396);
    pub const WHILE_STATEMENT: NodeKind = NodeKind(403);
    pub const RETURN_STATEMENT: NodeKind = NodeKind(410);
    pub const DO_WHILE_STATEMENT: NodeKind = NodeKind(404);
    pub const EXPRESSION_STATEMENT: NodeKind = NodeKind(400);
    pub const VAR_DECLARATION_STATEMENT: NodeKind = NodeKind(397);

    pub const TYPE_NAME: NodeKind = NodeKind(450);
    pub const VISIBILITY: NodeKind = NodeKind(413);
    pub const TYPE_ALIAS: NodeKind = NodeKind(533);
    pub const USING_ALIAS: NodeKind = NodeKind(370);
    pub const PRIMITIVE_TYPE: NodeKind = NodeKind(462);
    pub const ANY_SOURCE_TYPE: NodeKind = NodeKind(371);
    pub const RETURN_PARAMETER: NodeKind = NodeKind(455);
    pub const STATE_MUTABILITY: NodeKind = NodeKind(414);
    pub const VIRTUAL: NodeKind = NodeKind(178);
    pub const OVERRIDE_SPECIFIER: NodeKind = NodeKind(416);
    pub const RETURN_DEFINITION: NodeKind = NodeKind(421);
    pub const USER_DEFINED_TYPE: NodeKind = NodeKind(458);
    pub const USER_DEFINED_TYPE_DEFINITION: NodeKind = NodeKind(348);

    pub const EXPRESSION: NodeKind = NodeKind(427);
    pub const ARRAY_ACCESS: NodeKind = NodeKind(440);
    pub const SLICE_ACCESS: NodeKind = NodeKind(441);
    pub const INLINE_ARRAY_EXPRESSION: NodeKind = NodeKind(433);
    pub const CALL_ARGUMENT: NodeKind = NodeKind(424);
    pub const CALL_STRUCT_ARGUMENT: NodeKind = NodeKind(425);
    pub const EMIT_STATEMENT: NodeKind = NodeKind(411);
    pub const CALL_EXPRESSION: NodeKind = NodeKind(447);
    pub const CALL_OPTIONS: NodeKind = NodeKind(442);
    pub const NAMED_ARGUMENT: NodeKind = NodeKind(443);
    pub const REVERT_ARGUMENTS: NodeKind = NodeKind(532);
    pub const TUPLE_EXPRESSION: NodeKind = NodeKind(432);
    pub const REVERT_STATEMENT: NodeKind = NodeKind(407);
    pub const MEMBER_EXPRESSION: NodeKind = NodeKind(438);
    pub const INCOMPLETE_MEMBER_EXPRESSION: NodeKind = NodeKind(439);
    pub const MISSING_MEMBER_PROPERTY: NodeKind = NodeKind(531);
    pub const BINARY_EXPRESSION: NodeKind = NodeKind(434);
    pub const UNARY_EXPRESSION: NodeKind = NodeKind(435);
    pub const UPDATE_EXPRESSION: NodeKind = NodeKind(436);
    pub const TERNARY_EXPRESSION: NodeKind = NodeKind(430);
    pub const ASSIGNMENT_EXPRESSION: NodeKind = NodeKind(445);
    pub const AUGMENTED_ASSIGNMENT_EXPRESSION: NodeKind = NodeKind(446);
    pub const MODIFIER_INVOCATION: NodeKind = NodeKind(422);
    pub const TYPECAST_EXPRESSION: NodeKind = NodeKind(429);
    pub const META_TYPE_EXPRESSION: NodeKind = NodeKind(449);
    pub const PAYABLE_CONVERSION_EXPRESSION: NodeKind = NodeKind(448);
    pub const NEW_EXPRESSION: NodeKind = NodeKind(431);

    pub const STRING_LITERAL: NodeKind = NodeKind(470);
    pub const NUMBER_LITERAL: NodeKind = NodeKind(471);
    pub const BOOLEAN_LITERAL: NodeKind = NodeKind(477);
    pub const HEX_STRING_LITERAL: NodeKind = NodeKind(478);

    //     UN-NAMED NODES
    pub const AS: NodeKind = NodeKind(21);
    pub const TYPE: NodeKind = NodeKind(22);
    pub const ABSTRACT: NodeKind = NodeKind(25);
    pub const LPAREN: NodeKind = NodeKind(28);
    pub const GLOBAL: NodeKind = NodeKind(48);
    pub const MEMORY: NodeKind = NodeKind(153);
    pub const STORAGE: NodeKind = NodeKind(154);
    pub const CONSTANT: NodeKind = NodeKind(24);
    pub const CALLDATA: NodeKind = NodeKind(155);
    pub const IMMUTABLE: NodeKind = NodeKind(172);
    pub const PUBLIC: NodeKind = NodeKind(164);
    pub const INTERNAL: NodeKind = NodeKind(165);
    pub const PAYABLE: NodeKind = NodeKind(170);
    // ...
}

/// Each node kind is associated with specific fields (represented by field kind)
/// For example A contract has the following fields - name & body
/// An Import has the following fields - import name, alias & source
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct FieldKind(u16);

impl FieldKind {
    #[inline]
    pub const fn as_u16(self) -> u16 {
        self.0
    }
}

impl From<u16> for FieldKind {
    #[inline]
    fn from(id: u16) -> Self {
        Self(id)
    }
}

impl From<FieldKind> for u16 {
    #[inline]
    fn from(kind: FieldKind) -> Self {
        kind.0
    }
}

impl From<FieldKind> for std::num::NonZero<u16> {
    #[inline]
    fn from(kind: FieldKind) -> Self {
        std::num::NonZero::new(kind.0).expect("FieldKind must be non-zero")
    }
}

impl From<std::num::NonZero<u16>> for FieldKind {
    #[inline]
    fn from(id: std::num::NonZero<u16>) -> Self {
        Self(id.get())
    }
}

impl PartialEq<FieldKind> for u16 {
    #[inline]
    fn eq(&self, other: &FieldKind) -> bool {
        *self == other.0
    }
}

impl FieldKind {
    // Thin wrapper around TS field IDs from `enum ts_field_identifiers`
    pub const ALIAS: FieldKind = FieldKind(1);
    // pub const ANCESTOR: FieldKind = FieldKind(2);
    // pub const ANCESTOR_ARGUMENTS: FieldKind = FieldKind(3);
    // pub const ARGUMENT: FieldKind = FieldKind(4);
    // pub const ATTEMPT: FieldKind = FieldKind(5);
    pub const BASE: FieldKind = FieldKind(6);
    pub const BODY: FieldKind = FieldKind(7);
    // pub const CONDITION: FieldKind = FieldKind(8);
    // pub const ELSE: FieldKind = FieldKind(9);
    pub const ERROR: FieldKind = FieldKind(10);
    pub const FROM: FieldKind = FieldKind(11);
    pub const FUNCTION: FieldKind = FieldKind(12);
    pub const IMPORT_NAME: FieldKind = FieldKind(13);
    pub const INDEX: FieldKind = FieldKind(14);
    // pub const INITIAL: FieldKind = FieldKind(15);
    // pub const KEY_IDENTIFIER: FieldKind = FieldKind(16);
    pub const KEY_TYPE: FieldKind = FieldKind(17);
    pub const LEFT: FieldKind = FieldKind(18);
    pub const LOCATION: FieldKind = FieldKind(19);
    pub const NAME: FieldKind = FieldKind(20);
    pub const OBJECT: FieldKind = FieldKind(21);
    pub const OPERATOR: FieldKind = FieldKind(22);
    pub const PARAMETERS: FieldKind = FieldKind(23);
    pub const PROPERTY: FieldKind = FieldKind(24);
    pub const RETURN_TYPE: FieldKind = FieldKind(25);
    pub const RIGHT: FieldKind = FieldKind(26);
    pub const SOURCE: FieldKind = FieldKind(27);
    pub const TO: FieldKind = FieldKind(28);
    pub const TYPE: FieldKind = FieldKind(29);
    // pub const UPDATE: FieldKind = FieldKind(30);
    pub const VALUE: FieldKind = FieldKind(31);
    pub const VALUE_IDENTIFIER: FieldKind = FieldKind(32);
    pub const VALUE_TYPE: FieldKind = FieldKind(33);
    pub const VERSION_CONSTRAINT: FieldKind = FieldKind(34);
    pub const VISIBILITY: FieldKind = FieldKind(35);

}
