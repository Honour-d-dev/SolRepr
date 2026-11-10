use std::borrow::Cow;

use num_bigint::BigInt;

use crate::{
    ast::{ErrorId, EventId, FunctionId, UdvtId, VarId},
    hir::{
        exprs::Name,
        types::{BaseKind, CallKind, Callee, Fn, Location, Mutability, Primitive, Ty, TyKind, Type, Visibility},
    },
    ir::def_map::{DefNamespace, TypeDefId},
    salsa::HirDb,
};
use Primitive::*;

pub type StaticName = &'static str;

/// Macro so we can expand to an aggregate literal — the only expression shape rvalue
/// promotion accepts, so `BuiltinFn::spec` arms promote/cache to `.rodata`
/// instead of constructing the spec per call.
macro_rules! fn_spec {
    ($name:expr, $params:expr, $returns:expr, $schema:expr, $callkind:expr, $mutability:expr, $doc:expr $(,)?) => {
        &FnSpec {
            name: $name,
            params: $params,
            returns: $returns,
            schema: $schema,
            callkind: $callkind,
            mutability: $mutability,
            doc: $doc,
        }
    };
}

macro_rules! field_spec {
    ($name:expr, $prim:expr, $loc:expr, $doc:expr) => {
        &FieldSpec { name: $name, prim: Some($prim), loc: $loc, doc: $doc }
    };
}
macro_rules! meta_spec {
    ($name:expr, $prim:expr, $loc:expr) => {
        &FieldSpec { name: $name, prim: $prim, loc: $loc, doc: "" }
    };
}


/// How a builtin interprets its argument list.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ParamSchema {
    /// Fixed number of args
    Fixed,
    /// At least a fixed prefix, then remaining arguments must convert to a common type e.g`concat`.
    TypedTail,
    /// Anything goes for the tail — the `abi.encode*` functions.
    AnyTail,
    /// Special handling - The call is type-checked by a dedicated checker 
    /// - `require` can take an error/string/none as a second arg and
    /// - `abi.decode`/`encodeCall` take type names as args etc
    Intrinsic,
}

// ── Builtin identities ────────────────────────────────────────────────────


/// A builtin member - a `Copy` handle into the spec tables, never the spec itself.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    Obj(BuiltinTy),
    Fn(BuiltinFn),
    Field(BuiltinField),
}
use Builtin::*;

impl Builtin {
    pub const fn name(self) -> StaticName {
        match self {
            Obj(id) => id.name(),
            Fn(id) => id.name(),
            Field(id) => id.name(),
        }
    }

    pub fn doc(self, db: &dyn HirDb) -> Cow<'static, str> {
        match self {
            Obj(id) => id.doc().into(),
            Fn(id) => id.doc(db),
            Field(id) => id.doc(db),
        }
    }
}


#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltinTy {
    Msg,
    Block,
    Abi,
    Tx,
}

impl BuiltinTy {
    pub const fn name(self) -> StaticName {
        match self {
            Self::Msg => "msg",
            Self::Block => "block",
            Self::Abi => "abi",
            Self::Tx => "tx",
        }
    }

    pub const fn doc(self) -> &'static str {
        match self {
            Self::Msg => "Message context of the current call",
            Self::Block => "Context of the current block",
            Self::Abi => "ABI encoding and decoding functions",
            Self::Tx => "Transaction context",
        }
    }

    /// `msg`/`block`/`tx` read the environment; `abi` is a pure namespace.
    pub const fn reads_env(self) -> bool {
        matches!(self, Self::Msg | Self::Block | Self::Tx)
    }

    /// The object's member table — `msg.sender`, `abi.encode`, …
    pub const fn members(self) -> &'static [Builtin] {
        match self {
            Self::Msg => &MSG_MEMBERS,
            Self::Block => &BLOCK_MEMBERS,
            Self::Abi => &ABI_MEMBERS,
            Self::Tx => &TX_MEMBERS,
        }
    }
}

/// A builtin field member
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltinField {
    // `msg.*`
    MsgSender,
    MsgValue,
    MsgSig,
    MsgData,
    // `block.*`
    BlockNumber,
    BlockTimestamp,
    BlockCoinbase,
    BlockGaslimit,
    BlockDifficulty,
    BlockPrevrandao,
    BlockChainid,
    BlockBasefee,
    BlockBlobbasefee,
    // `tx.*`
    TxOrigin,
    TxGasprice,
    // `address.*`
    Balance,
    Code,
    Codehash,
    // `.length` — `uint8` on `bytesN`, `uint256` elsewhere.
    FixedBytesLength,
    BytesLength,
    ArrayLength,
    // `.address` on external function values.
    FnAddress,
    // `.selector` — the payload names the declaration for the hover doc; a
    // payload-less `FnPtrSelector` for `function(...) external` types.
    FnPtrSelector,
    FnSelector(FunctionId),
    GetterSelector(VarId),
    BuiltinFnSelector(BuiltinFn),
    ErrorSelector(ErrorId),
    EventSelector(EventId),
    // `type(T).*` — the payload is the `T` the metatype was formed over.
    MetaMin(Type),
    MetaMax(Type),
    MetaName(Type),
    CreationCode(Type),
    RuntimeCode(Type),
    InterfaceId(Type),
}


impl BuiltinField {
    /// The field's spec — every arm promotes to `.rodata`, so this returns
    /// a table pointer, never a constructed value.
    const fn spec(self) -> &'static FieldSpec {
        use Location::*;
        match self {
            Self::MsgSender => field_spec!("sender", Address, Stack, "The sender of the message (current call)"),
            Self::MsgValue => field_spec!("value", Uint(256), Stack, "Number of wei sent with the message"),
            Self::MsgSig => field_spec!("sig", FixedBytes(4), Stack, "First four bytes of the calldata (function selector)"),
            Self::MsgData => field_spec!("data", Bytes, Calldata, "Complete calldata"),
            Self::BlockNumber => field_spec!("number", Uint(256), Stack, "Current block number"),
            Self::BlockTimestamp => field_spec!("timestamp", Uint(256), Stack, "Current block timestamp (seconds since unix epoch)"),
            Self::BlockCoinbase => field_spec!("coinbase", AddressPayable, Stack, "Current block miner's address"),
            Self::BlockGaslimit => field_spec!("gaslimit", Uint(256), Stack, "Current block gas limit"),
            Self::BlockDifficulty => field_spec!("difficulty", Uint(256), Stack, "Current block difficulty (superseded by prevrandao after the Merge)"),
            Self::BlockPrevrandao => field_spec!("prevrandao", Uint(256), Stack, "Randomness beacon output of the previous block"),
            Self::BlockChainid => field_spec!("chainid", Uint(256), Stack, "ID of the current chain"),
            Self::BlockBasefee => field_spec!("basefee", Uint(256), Stack, "Current block's base fee"),
            Self::BlockBlobbasefee => field_spec!("blobbasefee", Uint(256), Stack, "Current block's blob base fee"),
            Self::TxOrigin => field_spec!("origin", Address, Stack, "Sender of the transaction (full call chain)"),
            Self::TxGasprice => field_spec!("gasprice", Uint(256), Stack, "Gas price of the transaction"),
            Self::Balance => field_spec!("balance", Uint(256), Stack, "Balance of the address in wei"),
            Self::Code => field_spec!("code", Bytes, Memory, "Code at the address (can be empty)"),
            Self::Codehash => field_spec!("codehash", FixedBytes(32), Stack, "The codehash of the address"),
            Self::FixedBytesLength => field_spec!("length", Uint(8), Stack, "Fixed length of the byte array"),
            Self::BytesLength => field_spec!("length", Uint(256), Stack, "length of the bytes"),
            Self::ArrayLength => field_spec!("length", Uint(256), Stack, "Number of elements in the array"),
            Self::FnAddress => field_spec!("address", Address, Stack, "The contract this function belongs to"),
            Self::FnPtrSelector => field_spec!("selector", FixedBytes(4), Stack, "ABI selector of the function"),
            Self::FnSelector(_)
            | Self::GetterSelector(_)
            | Self::BuiltinFnSelector(_)
            | Self::ErrorSelector(_) => meta_spec!("selector", Some(FixedBytes(4)), Stack),
            Self::EventSelector(_) => meta_spec!("selector", Some(FixedBytes(32)), Stack),
            Self::MetaMin(_) => meta_spec!("min", None, Stack),
            Self::MetaMax(_) => meta_spec!("max", None, Stack),
            Self::MetaName(_) => meta_spec!("name", Some(String), Memory),
            Self::CreationCode(_) => meta_spec!("creationCode", Some(Bytes), Memory),
            Self::RuntimeCode(_) => meta_spec!("runtimeCode", Some(Bytes), Memory),
            Self::InterfaceId(_) => meta_spec!("interfaceId", Some(FixedBytes(4)), Stack),
        }
    }

    pub const fn name(self) -> StaticName {
        self.spec().name
    }

    /// The field's declared type, located. `type(T).min`/`max` answer the
    /// metatype's operand itself — `type(E).min` is an `E`, not a `uint8`.
    pub fn ty(self, db: &dyn HirDb) -> Type {
        let (Self::MetaMin(ty) | Self::MetaMax(ty)) = self else {
            let spec = self.spec();
            let Some(prim) = spec.prim else {
                unreachable!("{} carries no primitive spec", spec.name)
            };
            return Type::primitive(db, prim).at_loc(db, spec.loc);
        };
        ty
    }

    /// `a.balance`/`a.code`/`a.codehash` read the *account* — an environment
    /// access independent of `a`'s own location.
    pub const fn reads_env(self) -> bool {
        matches!(self, Self::Balance | Self::Code | Self::Codehash)
    }

    /// Hover docs — static for table fields, spelled from the payload for
    /// the members a receiver specializes.
    pub fn doc(self, db: &dyn HirDb) -> Cow<'static, str> {
        match self {
            Self::FnSelector(id) => {
                format!("ABI selector of `{}`", db.function_data(id).name).into()
            }
            Self::GetterSelector(id) => {
                format!("ABI selector of `{}`", db.var_data(id).name).into()
            }
            Self::BuiltinFnSelector(id) => format!("ABI selector of `{}`", id.name()).into(),
            Self::ErrorSelector(id) => {
                format!("ABI selector of `{}`", db.error_data(id).name).into()
            }
            Self::EventSelector(id) => {
                format!("Topic-0 hash of `{}`", db.event_data(id).name).into()
            }
            Self::MetaMin(ty) => match ty.kind(db) {
                TyKind::TypeDef(TypeDefId::Enum(_)) => {
                    format!("The smallest variant of `{}`", ty.display(db)).into()
                }
                _ => format!("The minimum value representable by `{}`", ty.display(db)).into(),
            },
            Self::MetaMax(ty) => match ty.kind(db) {
                TyKind::TypeDef(TypeDefId::Enum(_)) => {
                    format!("The largest variant of `{}`", ty.display(db)).into()
                }
                _ => format!("The maximum value representable by `{}`", ty.display(db)).into(),
            },
            Self::MetaName(ty) => {
                format!("Name of the {} `{}`", Self::meta_noun(db, ty), ty.display(db)).into()
            }
            Self::CreationCode(ty) => {
                format!("Creation bytecode of the {} `{}`", Self::meta_noun(db, ty), ty.display(db))
                    .into()
            }
            Self::RuntimeCode(ty) => {
                format!("Runtime bytecode of the {} `{}`", Self::meta_noun(db, ty), ty.display(db))
                    .into()
            }
            Self::InterfaceId(ty) => {
                format!("EIP-165 interface identifier of the interface `{}`", ty.display(db))
                    .into()
            }
            _ => self.spec().doc.into(),
        }
    }

    /// The noun `type(T)` docs use for `T` — "contract `C`", "interface `I`".
    fn meta_noun(db: &dyn HirDb, ty: Type) -> &'static str {
        match ty.kind(db) {
            TyKind::TypeDef(TypeDefId::Contract(_)) => "contract",
            TyKind::TypeDef(TypeDefId::Interface(_)) => "interface",
            TyKind::TypeDef(TypeDefId::Library(_)) => "library",
            _ => "type",
        }
    }
}



/// A builtin function 
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltinFn {
    // Free functions.
    Assert,
    Require,
    Keccak256,
    Sha256,
    Ripemd160,
    Ecrecover,
    Addmod,
    Mulmod,
    Gasleft,
    Blockhash,
    Blobhash,
    Selfdestruct,
    Now,
    // `abi.*`
    Encode,
    EncodePacked,
    EncodeWithSelector,
    EncodeWithSignature,
    EncodeCall,
    Decode,
    // `bytes.concat` / `string.concat`
    BytesConcat,
    StringConcat,
    // `address` / `address payable` members.
    Call,
    Delegatecall,
    Staticcall,
    Send,
    Transfer,
    // Storage `bytes` members — the element type is fixed at `bytes1`.
    BytesPushVal,
    BytesPushEmpty,
    BytesPop,
    // Storage `T[]` members — the element `Type` parameterizes the
    // signature and the hover doc.
    ArrayPushVal(Type),
    ArrayPushEmpty(Type),
    ArrayPop(Type),
    // UDVT members, parameterized by the declaration.
    Wrap(UdvtId),
    Unwrap(UdvtId),
}


impl BuiltinFn {
    const fn spec(self) -> &'static FnSpec {
        use CallKind::*;
        use Mutability::*;
        use ParamSchema::*;
        match self {
            Self::Assert => fn_spec!("assert", &[Boolean], &[], Fixed, Static, Pure, "Throws if the condition is false"),
            Self::Require => fn_spec!("require", &[Boolean], &[], Intrinsic, Static, Pure, "Reverts if the condition is false with a string message or a custom error"),
            Self::Keccak256 => fn_spec!("keccak256", &[Bytes], &[FixedBytes(32)], Fixed, Static, Pure, "Computes the Keccak-256 hash"),
            Self::Sha256 => fn_spec!("sha256", &[Bytes], &[FixedBytes(32)], Fixed, Static, Pure, "Computes the SHA-256 hash"),
            Self::Ripemd160 => fn_spec!("ripemd160", &[Bytes], &[FixedBytes(20)], Fixed, Static, Pure, "Computes the RIPEMD-160 hash"),
            Self::Ecrecover => fn_spec!(
                "ecrecover",
                &[FixedBytes(32), Uint(8), FixedBytes(32), FixedBytes(32)],
                &[Address],
                Fixed, Static, Pure,
                "Recovers the address that signed a message",
            ),
            Self::Addmod => fn_spec!("addmod", &[Uint(256); 3], &[Uint(256)], Fixed, Static, Pure, "Computes addition modulo a modulus"),
            Self::Mulmod => fn_spec!("mulmod", &[Uint(256); 3], &[Uint(256)], Fixed, Static, Pure, "Computes multiplication modulo a modulus"),
            Self::Gasleft => fn_spec!("gasleft", &[], &[Uint(256)], Fixed, Static, View, "Returns the remaining gas"),
            Self::Blockhash => fn_spec!("blockhash", &[Uint(256)], &[FixedBytes(32)], Fixed, Static, View, "Returns the hash of a recent block"),
            Self::Blobhash => fn_spec!("blobhash", &[Uint(256)], &[FixedBytes(32)], Fixed, Static, View, "Returns the versioned hash of the index-th blob"),
            Self::Selfdestruct => fn_spec!("selfdestruct", &[AddressPayable], &[], Fixed, Static, NonPayable, "Destroys the current contract"),
            // `now` aliases `block.timestamp`, an environment read.
            Self::Now => fn_spec!("now", &[], &[Uint(256)], Fixed, Static, View, "Deprecated alias for block.timestamp"),
            Self::Encode => fn_spec!("encode", &[], &[Bytes], AnyTail, Static, Pure, "ABI-encodes the given arguments"),
            Self::EncodePacked => fn_spec!("encodePacked", &[], &[Bytes], AnyTail, Static, Pure, "ABI-encodes the given arguments with non-standard packed mode"),
            Self::EncodeWithSelector => fn_spec!("encodeWithSelector", &[FixedBytes(4)], &[Bytes], AnyTail, Static, Pure, "ABI-encodes with a 4-byte selector"),
            Self::EncodeWithSignature => fn_spec!("encodeWithSignature", &[String], &[Bytes], AnyTail, Static, Pure, "ABI-encodes with a function signature"),
            Self::EncodeCall => fn_spec!("encodeCall", &[], &[Bytes], Intrinsic, Static, Pure, "ABI-encodes a function call"),
            Self::Decode => fn_spec!("decode", &[Bytes], &[], Intrinsic, Static, Pure, "ABI-decodes the given data into the type tuple given as the second argument"),
            Self::BytesConcat => fn_spec!("concat", &[Bytes], &[Bytes], TypedTail, Static, Pure, "Concatenates multiple bytes arrays"),
            Self::StringConcat => fn_spec!("concat", &[String], &[String], TypedTail, Static, Pure, "Concatenates multiple strings"),
            Self::Call => fn_spec!("call", &[Bytes], &[Boolean, Bytes], Fixed, External, Payable, "Low-level CALL with given calldata"),
            Self::Delegatecall => fn_spec!("delegatecall", &[Bytes], &[Boolean, Bytes], Fixed, External, NonPayable, "Low-level DELEGATECALL with given calldata"),
            Self::Staticcall => fn_spec!("staticcall", &[Bytes], &[Boolean, Bytes], Fixed, External, View, "Low-level STATICCALL with given calldata"),
            Self::Send => fn_spec!("send", &[Uint(256)], &[Boolean], Fixed, External, NonPayable, "Sends Ether to the address, returns false on failure"),
            Self::Transfer => fn_spec!("transfer", &[Uint(256)], &[], Fixed, External, NonPayable, "Sends Ether to the address, reverts on failure"),
            Self::BytesPushVal => fn_spec!("push", &[FixedBytes(1)], &[], Fixed, Static, NonPayable, "Pushes a `bytes1` to the end of the byte array"),
            Self::BytesPushEmpty => fn_spec!("push", &[], &[FixedBytes(1)], Fixed, Static, NonPayable, "Appends a zero-initialised `bytes1` and returns a reference to it"),
            Self::BytesPop => fn_spec!("pop", &[], &[], Fixed, Static, NonPayable, "Removes and deletes the last `bytes1` from the byte array"),
            // The payload supplies params/returns and the doc; only the
            // fixed fields live in the spec.
            Self::ArrayPushVal(_) => fn_spec!("push", &[], &[], Fixed, Static, NonPayable, ""),
            Self::ArrayPushEmpty(_) => fn_spec!("push", &[], &[], Fixed, Static, NonPayable, ""),
            Self::ArrayPop(_) => fn_spec!("pop", &[], &[], Fixed, Static, NonPayable, ""),
            Self::Wrap(_) => fn_spec!("wrap", &[], &[], Fixed, Static, Pure, ""),
            Self::Unwrap(_) => fn_spec!("unwrap", &[], &[], Fixed, Static, Pure, ""),
        }
    }

    pub const fn name(self) -> StaticName {
        self.spec().name
    }

    pub const fn schema(self) -> ParamSchema {
        self.spec().schema
    }

    pub const fn callkind(self) -> CallKind {
        self.spec().callkind
    }

    pub const fn mutability(self) -> Mutability {
        self.spec().mutability
    }

    /// The call options this builtin admits as a host — solc gates options
    /// on `FunctionType::Kind`, and the id *is* that tag: only the low-level
    /// `address` calls take options.
    pub const fn options(self) -> &'static [StaticName] {
        match self {
            Self::Call => &["value", "gas"],
            Self::Delegatecall | Self::Staticcall => &["gas"],
            _ => &[],
        }
    }

    /// The builtins solc folds in constant expressions.
    pub const fn is_const(self) -> bool {
        matches!(
            self,
            Self::Keccak256 | Self::Sha256 | Self::Ripemd160 | Self::Ecrecover | Self::Addmod | Self::Mulmod
        )
    }

    /// Materializes `(params, returns)` — params unlocated, returns upcast
    /// to their default data location. The parameterized variants spell
    /// their signature from the payload.
    fn types(self, db: &dyn HirDb) -> (Box<[Type]>, Box<[Type]>) {
        let udvt = |id: UdvtId| {
            (
                Type::primitive(db, db.udvt_data(id).underlying),
                Type::new(db, TyKind::TypeDef(id.into())).expect("UDVT is nameable"),
            )
        };
        match self {
            Self::ArrayPushVal(elem) => (Box::new([elem.upcast(db)]), Box::new([])),
            Self::ArrayPushEmpty(elem) => (Box::new([]), Box::new([elem.upcast(db)])),
            Self::ArrayPop(_) => (Box::new([]), Box::new([])),
            Self::Wrap(id) => {
                let (underlying, udvt) = udvt(id);
                (Box::new([underlying]), Box::new([udvt]))
            }
            Self::Unwrap(id) => {
                let (underlying, udvt) = udvt(id);
                (Box::new([udvt]), Box::new([underlying]))
            }
            _ => {
                let spec = self.spec();
                let params = spec
                    .params
                    .iter()
                    .map(|&prim| Type::primitive(db, prim))
                    .collect();
                let returns = spec
                    .returns
                    .iter()
                    .map(|&prim| Type::primitive(db, prim))
                    .collect();
                (params, returns)
            }
        }
    }

    /// [`Self::pointer_sig`]
    pub fn call_sig(self, db: &dyn HirDb) -> Fn<Type> {
        let (params, returns) = self.types(db);
        Fn {
            visibility: match self.callkind() {
                CallKind::External => Visibility::External,
                _ => Visibility::Internal,
            },
            mutability: self.mutability(),
            params,
            returns,
        }
    }

    /// The function-pointer *type* of a builtin that is a value — only the
    /// entries callable through an instance (`address.call`, …) are values;
    /// every other yields `None`, which is what stops `keccak256` or
    /// `abi.encode` converting to a function pointer.
    pub fn pointer_sig(self, db: &dyn HirDb) -> Option<Fn<Type>> {
        (self.callkind() == CallKind::External).then(|| self.call_sig(db))
    }

    /// Hover docs — static for table builtins, spelled from the payload for
    /// the receiver-parameterized ones.
    pub fn doc(self, db: &dyn HirDb) -> Cow<'static, str> {
        match self {
            Self::Wrap(id) => {
                let data = db.udvt_data(id);
                let underlying = Ty::primitive(db, data.underlying);
                format!("Wraps a {} into {}", underlying.display(db), data.name).into()
            }
            Self::Unwrap(id) => {
                let data = db.udvt_data(id);
                let underlying = Ty::primitive(db, data.underlying);
                format!("Unwraps the {} into its underlying {}", data.name, underlying.display(db))
                    .into()
            }
            Self::ArrayPushVal(elem) => {
                format!("Pushes a {} to the end of the array", elem.display(db)).into()
            }
            Self::ArrayPushEmpty(elem) => format!(
                "Appends a zero-initialised {} and returns a reference to it",
                elem.display(db)
            )
            .into(),
            Self::ArrayPop(elem) => format!(
                "Removes the last {} from the array, deleting it",
                elem.display(db)
            )
            .into(),
            _ => self.spec().doc.into(),
        }
    }
}

// GLOBAL and BUILTIN OBJECTS/SYMBOLS
static GLOBALS: [Builtin; 17] = [
    Obj(BuiltinTy::Msg),
    Obj(BuiltinTy::Block),
    Obj(BuiltinTy::Abi),
    Obj(BuiltinTy::Tx),
    Fn(BuiltinFn::Assert),
    Fn(BuiltinFn::Require),
    Fn(BuiltinFn::Keccak256),
    Fn(BuiltinFn::Sha256),
    Fn(BuiltinFn::Ripemd160),
    Fn(BuiltinFn::Ecrecover),
    Fn(BuiltinFn::Addmod),
    Fn(BuiltinFn::Mulmod),
    Fn(BuiltinFn::Gasleft),
    Fn(BuiltinFn::Blockhash),
    Fn(BuiltinFn::Blobhash),
    Fn(BuiltinFn::Selfdestruct),
    Fn(BuiltinFn::Now),
];

static MSG_MEMBERS: [Builtin; 4] = [
    Field(BuiltinField::MsgSender),
    Field(BuiltinField::MsgValue),
    Field(BuiltinField::MsgSig),
    Field(BuiltinField::MsgData),
];

static BLOCK_MEMBERS: [Builtin; 9] = [
    Field(BuiltinField::BlockNumber),
    Field(BuiltinField::BlockTimestamp),
    Field(BuiltinField::BlockCoinbase),
    Field(BuiltinField::BlockGaslimit),
    Field(BuiltinField::BlockDifficulty),
    Field(BuiltinField::BlockPrevrandao),
    Field(BuiltinField::BlockChainid),
    Field(BuiltinField::BlockBasefee),
    Field(BuiltinField::BlockBlobbasefee),
];

static TX_MEMBERS: [Builtin; 2] = [
    Field(BuiltinField::TxOrigin),
    Field(BuiltinField::TxGasprice),
];

static ABI_MEMBERS: [Builtin; 6] = [
    Fn(BuiltinFn::Encode),
    Fn(BuiltinFn::EncodePacked),
    Fn(BuiltinFn::EncodeWithSelector),
    Fn(BuiltinFn::EncodeWithSignature),
    Fn(BuiltinFn::EncodeCall),
    Fn(BuiltinFn::Decode),
];

/// Members common to both `address` and `address payable`.
static ADDRESS_MEMBERS: [Builtin; 6] = [
    Field(BuiltinField::Balance),
    Field(BuiltinField::Code),
    Field(BuiltinField::Codehash),
    Fn(BuiltinFn::Call),
    Fn(BuiltinFn::Delegatecall),
    Fn(BuiltinFn::Staticcall),
];

/// `.send`/`.transfer` exist only on `address payable`.
static ADDRESS_PAYABLE_MEMBERS: [Builtin; 8] = [
    Field(BuiltinField::Balance),
    Field(BuiltinField::Code),
    Field(BuiltinField::Codehash),
    Fn(BuiltinFn::Call),
    Fn(BuiltinFn::Delegatecall),
    Fn(BuiltinFn::Staticcall),
    Fn(BuiltinFn::Transfer),
    Fn(BuiltinFn::Send),
];

/// `bytes1`..`bytes32` expose a read-only `.length`.
static FIXED_BYTES_MEMBERS: [Builtin; 1] = [Field(BuiltinField::FixedBytesLength)];

static BYTES_LENGTH_MEMBERS: [Builtin; 1] = [Field(BuiltinField::BytesLength)];

static ARRAY_LENGTH_MEMBERS: [Builtin; 1] = [Field(BuiltinField::ArrayLength)];

/// Storage `bytes` behave like a `bytes1[]`: `.push`/`.pop` are available
/// only in storage. The element type is `bytes1`.
static BYTES_STORAGE_MEMBERS: [Builtin; 4] = [
    Field(BuiltinField::BytesLength),
    Fn(BuiltinFn::BytesPushEmpty),
    Fn(BuiltinFn::BytesPushVal),
    Fn(BuiltinFn::BytesPop),
];

static BYTES_TYPE_MEMBERS: [Builtin; 1] = [Fn(BuiltinFn::BytesConcat)];

static STRING_TYPE_MEMBERS: [Builtin; 1] = [Fn(BuiltinFn::StringConcat)];

static FN_EXTERNAL_MEMBERS: [Builtin; 2] =
    [Field(BuiltinField::FnPtrSelector), Field(BuiltinField::FnAddress)];

// MARK: members

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MemberNamespace {
    Types,
    Values,
}

impl From<DefNamespace> for MemberNamespace {
    fn from(value: DefNamespace) -> Self {
        match value {
            DefNamespace::Types => Self::Types,
            _ => Self::Values,
        }
    }
}

/// Builtin member resolution, database-aware. Owns the builtin tables
/// and the name filtering. `Copy` itself: a `BuiltinDb::new(db)` on the fly costs nothing.
#[derive(Clone, Copy)]
pub struct BuiltinDb<'db> {
    db: &'db dyn HirDb,
}

impl<'db> BuiltinDb<'db> {
    pub fn new(db: &'db dyn HirDb) -> Self {
        Self { db }
    }

    /// All global builtins (`msg`, `block`, `abi`, `tx`, and free functions).
    pub fn globals(&self) -> &'static [Builtin] {
        &GLOBALS
    }

    
    pub fn resolve_global(&self, name: &Name) -> Option<Builtin> {
        GLOBALS
            .iter()
            .find(|global| global.name() == name.as_str())
            .copied()
    }

    /// Every builtin member of `ty` in `ctx` — `Types` is member access on
    /// a *type name* (`bytes.concat`, `T.wrap`), `Values` is member access
    /// on a *value* (`msg.sender`, `arr.push`, `f.selector`). Returned as a
    /// [`Cow`]: `Borrowed` for the static tables (the common case), `Owned`
    /// for the receiver-parameterized sets (`T.wrap`, `T[].push`,
    /// `type(T).*`, `f.selector`).
    pub fn members(&self, ty: Ty, ctx: MemberNamespace) -> Cow<'static, [Builtin]> {
        match ctx {
            MemberNamespace::Types => self.type_members(ty),
            MemberNamespace::Values => self.value_members(ty),
        }
    }

    /// Members of a `Types` receiver — `bytes.concat`, `T.wrap`,
    /// `type(E).max`, `msg.sender`.
    fn type_members(&self, ty: Ty) -> Cow<'static, [Builtin]> {
        match ty.peel_loc(self.db).kind(self.db) {
            TyKind::Meta(inner) => self.meta_type_members(inner.peel_loc(self.db)),
            TyKind::Builtin(id) => Cow::Borrowed(id.members()),
            TyKind::Primitive(Primitive::Bytes) => Cow::Borrowed(&BYTES_TYPE_MEMBERS),
            TyKind::Primitive(Primitive::String) => Cow::Borrowed(&STRING_TYPE_MEMBERS),
            TyKind::TypeDef(TypeDefId::Udvt(id)) => {
                Cow::Owned(vec![Fn(BuiltinFn::Wrap(id)), Fn(BuiltinFn::Unwrap(id))])
            }
            _ => Cow::Borrowed(&[]),
        }
    }

    /// Members of a `Values` receiver. The `Ref` wrapper is peeled
    /// for the base kind, but the location survives for `push`/`pop` gating.
    fn value_members(&self, ty: Ty) -> Cow<'static, [Builtin]> {
        let loc = ty.loc(self.db);
        match ty.peel_loc(self.db).kind(self.db) {
            TyKind::Primitive(Primitive::Address) => Cow::Borrowed(&ADDRESS_MEMBERS),
            TyKind::Primitive(Primitive::AddressPayable) => {
                Cow::Borrowed(&ADDRESS_PAYABLE_MEMBERS)
            }
            TyKind::Primitive(Primitive::Bytes) => self.bytes_members(loc),
            // `string` has no members in Solidity (`s.length` is an error).
            TyKind::Primitive(Primitive::String) => Cow::Borrowed(&[]),
            TyKind::Primitive(Primitive::FixedBytes(_)) => Cow::Borrowed(&FIXED_BYTES_MEMBERS),
            TyKind::Array { elem, size } => self.array_members(loc, size, &elem),
            TyKind::FnPtr(sig) => self.fn_ptr_members(sig.visibility),
            TyKind::Function { callee, kind } => self.function_members(callee, kind),
            TyKind::Error(id) => Cow::Owned(vec![Field(BuiltinField::ErrorSelector(id))]),
            TyKind::Event(id) => {
                if self.db.event_data(id).anonymous {
                    Cow::Borrowed(&[])
                } else {
                    Cow::Owned(vec![Field(BuiltinField::EventSelector(id))])
                }
            }
            _ => Cow::Borrowed(&[]),
        }
    }

    /// `type(T)` members — `type(uint).max`, `type(C).creationCode`,
    /// `type(E).min`, `type(I).interfaceId`, and `type(...).name`.
    fn meta_type_members(&self, ty: Type) -> Cow<'static, [Builtin]> {
        use BuiltinField::*;
        match ty.peel_loc(self.db).kind(self.db) {
            // `type(T).min`/`max` for value types
            TyKind::Primitive(Primitive::Uint(_) | Primitive::Int(_))
            | TyKind::TypeDef(TypeDefId::Enum(_)) => {
                Cow::Owned(vec![Field(MetaMin(ty)), Field(MetaMax(ty))])
            }
            // `type(C).name`/`creationCode`/`runtimeCode`
            TyKind::TypeDef(TypeDefId::Contract(_)) | TyKind::TypeDef(TypeDefId::Library(_)) => {
                Cow::Owned(vec![
                    Field(MetaName(ty)),
                    Field(CreationCode(ty)),
                    Field(RuntimeCode(ty)),
                ])
            }
            // `type(I).name`/`interfaceId`
            TyKind::TypeDef(TypeDefId::Interface(_)) => {
                Cow::Owned(vec![Field(MetaName(ty)), Field(InterfaceId(ty))])
            }
            _ => Cow::Borrowed(&[]),
        }
    }

    /// `.selector`/`.address` for a function type, gated by how it was
    /// reached (`this.f`/`c.f` vs a type name vs `using`) and whether the
    /// function is part of the external interface. Free and internal
    /// library functions are `Internal`, so they get nothing.
    fn function_members(&self, callee: Callee, kind: CallKind) -> Cow<'static, [Builtin]> {
        // A builtin has no declaration, but its name still names the
        // member — `address(x).call.selector` is legal, so an external
        // builtin pointer gets the same members as an external declared
        // one.
        let selector = match callee {
            Callee::Fn(id) => Field(BuiltinField::FnSelector(id)),
            Callee::Getter(id) => Field(BuiltinField::GetterSelector(id)),
            Callee::Builtin(id) => Field(BuiltinField::BuiltinFnSelector(id)),
            Callee::Constructor(_) | Callee::Allocation(_) => return Cow::Borrowed(&[]),
        };
        match kind {
            CallKind::External => {
                Cow::Owned(vec![selector, Field(BuiltinField::FnAddress)])
            }
            // Function type reached via the contract type name or `using` —
            // solc gives `.selector` when the function is part of the
            // external interface.
            CallKind::ViaType(_)
            | CallKind::Bound
            | CallKind::Base(BaseKind::Explicit | BaseKind::Super)
                if matches!(callee, Callee::Fn(id)
                        if matches!(self.db.function_data(id).vis, Visibility::Public | Visibility::External)) =>
            {
                Cow::Owned(vec![selector])
            }
            // `C.f` where `f` is declared in `C` itself: an `external`
            // function (or a bodyless interface-style `public`) gets
            // `.selector`; an implemented `public` stays internal until it
            // becomes a pointer.
            CallKind::Base(BaseKind::SelfQualified)
                if matches!(callee, Callee::Fn(id) if {
                    let data = self.db.function_data(id);
                    data.vis == Visibility::External
                        || (data.vis == Visibility::Public && !data.has_body)
                }) =>
            {
                Cow::Owned(vec![selector])
            }
            _ => Cow::Borrowed(&[]),
        }
    }

    /// Array members. `.length` is available on every array; `.push`/`.pop`
    /// only on dynamic arrays stored in storage (solc: "Member `push` is
    /// not available in `uint256[] memory` outside of storage").
    fn array_members(
        &self,
        loc: Option<Location>,
        size: Option<BigInt>,
        elem: &Type,
    ) -> Cow<'static, [Builtin]> {
        if size.is_some() || !loc.is_some_and(Location::is_storage) {
            return Cow::Borrowed(&ARRAY_LENGTH_MEMBERS);
        }
        Cow::Owned(vec![
            Field(BuiltinField::ArrayLength),
            Fn(BuiltinFn::ArrayPushVal(*elem)),
            Fn(BuiltinFn::ArrayPushEmpty(*elem)),
            Fn(BuiltinFn::ArrayPop(*elem)),
        ])
    }

    /// `bytes` behaves like `bytes1[]`: `.length` always, `.push`/`.pop`
    /// only in storage.
    fn bytes_members(&self, loc: Option<Location>) -> Cow<'static, [Builtin]> {
        if !loc.is_some_and(Location::is_storage) {
            return Cow::Borrowed(&BYTES_LENGTH_MEMBERS);
        }
        Cow::Borrowed(&BYTES_STORAGE_MEMBERS)
    }

    /// `function(...) external` value members — `f.selector`, `f.address`.
    fn fn_ptr_members(&self, visibility: Visibility) -> Cow<'static, [Builtin]> {
        if visibility == Visibility::External {
            Cow::Borrowed(&FN_EXTERNAL_MEMBERS)
        } else {
            Cow::Borrowed(&[])
        }
    }
}

// ── Specs ─────────────────────────────────────────────────────────────────
// Private table facts — callers never touch a spec; they ask the id.

struct FnSpec {
    name: StaticName,
    params: &'static [Primitive],
    returns: &'static [Primitive],
    schema: ParamSchema,
    callkind: CallKind,
    mutability: Mutability,
    doc: &'static str,
}


struct FieldSpec {
    name: StaticName,
    prim: Option<Primitive>,
    loc: Location,
    doc: &'static str,
}



