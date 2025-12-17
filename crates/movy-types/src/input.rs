use std::{
    fmt::{self, Display},
    str::FromStr,
};

use alloy_primitives::{B256, U128, U256};
use move_core_types::{account_address::AccountAddress, language_storage::StructTag};
use serde::{Deserialize, Serialize};
use sui_types::{
    Identifier, TypeTag,
    base_types::{ObjectID, ObjectRef, SequenceNumber, SuiAddress},
    programmable_transaction_builder::ProgrammableTransactionBuilder,
    transaction::{
        Argument, Command, ObjectArg, ProgrammableMoveCall, ProgrammableTransaction,
        SharedObjectMutability,
    },
    type_input::{StructInput, TypeInput},
};

use crate::{abi::MoveModuleId, error::MovyError};

#[derive(
    Copy, Debug, Clone, Hash, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Default,
)]
#[serde(transparent)]
pub struct MoveAddress(B256);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FunctionIdent(pub MoveModuleId, pub String);

impl FunctionIdent {
    pub fn new(module_address: &MoveAddress, module_name: &str, function_name: &str) -> Self {
        Self(
            MoveModuleId {
                module_address: *module_address,
                module_name: module_name.to_string(),
            },
            function_name.to_string(),
        )
    }
}

impl FromStr for FunctionIdent {
    type Err = MovyError;
    fn from_str(s: &str) -> Result<Self, MovyError> {
        let parts: Vec<&str> = s.split("::").collect();
        if parts.len() != 3 {
            return Err(MovyError::InvalidIdentifier(format!(
                "Invalid FunctionIdent string: {}",
                s
            )));
        }
        let module_address = MoveAddress::from_str(parts[0])?;
        let module_name = parts[1].to_string();
        let function_name = parts[2].to_string();
        Ok(FunctionIdent(
            MoveModuleId {
                module_address,
                module_name,
            },
            function_name,
        ))
    }
}

impl Display for FunctionIdent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}::{}", self.0, self.1)
    }
}

impl From<MoveAddress> for ObjectID {
    fn from(value: MoveAddress) -> Self {
        Self::new(value.0.0)
    }
}
impl From<MoveAddress> for SuiAddress {
    fn from(value: MoveAddress) -> Self {
        Self::from(ObjectID::from(value))
    }
}
impl From<MoveAddress> for AccountAddress {
    fn from(value: MoveAddress) -> Self {
        Self::new(value.0.0)
    }
}
impl From<AccountAddress> for MoveAddress {
    fn from(value: AccountAddress) -> Self {
        Self(B256::new(value.into_bytes()))
    }
}
impl From<SuiAddress> for MoveAddress {
    fn from(value: SuiAddress) -> Self {
        Self(value.to_inner().into())
    }
}

impl From<ObjectID> for MoveAddress {
    fn from(value: ObjectID) -> Self {
        Self(B256::new(value.into_bytes()))
    }
}

impl MoveAddress {
    pub fn random() -> Self {
        ObjectID::random().into()
    }
    pub fn to_canonical_string(&self, with_prefix: bool) -> String {
        ObjectID::from(*self).to_canonical_string(with_prefix)
    }

    pub fn is_sui_std(&self) -> bool {
        let address: AccountAddress = (*self).into();
        address == AccountAddress::ONE
            || address == AccountAddress::TWO
            || address == AccountAddress::from_suffix(3)
            || address == AccountAddress::from_suffix(13)
    }

    pub fn short(&self) -> String {
        let bs = &self.0.0;
        format!(
            "0x{}..{}",
            const_hex::encode(&bs[0..2]),
            const_hex::encode(&bs[30..32])
        )
    }

    pub fn from_str(s: &str) -> Result<Self, MovyError> {
        let Ok(object_id) = ObjectID::from_str(s) else {
            return Err(MovyError::InvalidIdentifier(format!(
                "Invalid MoveAddress string: {}",
                s
            )));
        };
        Ok(Self::from(object_id))
    }

    pub fn zero() -> Self {
        Self(B256::ZERO)
    }

    pub fn is_zero(&self) -> bool {
        self.0 == B256::ZERO
    }

    pub fn one() -> Self {
        Self::from(AccountAddress::ONE)
    }

    pub fn two() -> Self {
        Self::from(AccountAddress::TWO)
    }
}

impl Display for MoveAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if f.alternate() {
            f.write_str(&self.short())
        } else {
            f.write_str(&self.to_canonical_string(true))
        }
    }
}

impl FromStr for MoveAddress {
    type Err = MovyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        MoveAddress::from_str(s)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SuiObjectInputArgument {
    ImmOrOwnedObject(ObjectRef),
    SharedObject {
        id: ObjectID,
        initial_shared_version: SequenceNumber,
        mutable: bool,
    },
    Receiving(ObjectRef),
}

impl SuiObjectInputArgument {
    pub fn imm_or_owned_object(id: MoveAddress, version: u64, digest: [u8; 32]) -> Self {
        Self::ImmOrOwnedObject((id.into(), version.into(), digest.into()))
    }

    pub fn receiving(id: MoveAddress, version: u64, digest: [u8; 32]) -> Self {
        Self::Receiving((id.into(), version.into(), digest.into()))
    }

    pub fn shared_object(id: MoveAddress, initial_shared_version: u64, mutable: bool) -> Self {
        Self::SharedObject {
            id: id.into(),
            initial_shared_version: initial_shared_version.into(),
            mutable,
        }
    }

    pub fn id(&self) -> ObjectID {
        match self {
            SuiObjectInputArgument::ImmOrOwnedObject((id, _, _)) => *id,
            SuiObjectInputArgument::Receiving((id, _, _)) => *id,
            SuiObjectInputArgument::SharedObject { id, .. } => *id,
        }
    }
}

impl From<SuiObjectInputArgument> for ObjectArg {
    fn from(value: SuiObjectInputArgument) -> Self {
        match value {
            SuiObjectInputArgument::ImmOrOwnedObject(v) => Self::ImmOrOwnedObject(v),
            SuiObjectInputArgument::Receiving(v) => Self::Receiving(v),
            SuiObjectInputArgument::SharedObject {
                id,
                initial_shared_version,
                mutable,
            } => Self::SharedObject {
                id,
                initial_shared_version,
                mutability: if mutable {
                    SharedObjectMutability::Mutable
                } else {
                    SharedObjectMutability::Immutable
                },
            },
        }
    }
}

impl Display for SuiObjectInputArgument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SuiObjectInputArgument::ImmOrOwnedObject((id, _, _)) => {
                write!(f, "ImmOrOwnedObject({})", id)
            }
            SuiObjectInputArgument::Receiving((id, _, _)) => write!(f, "Receiving({})", id),
            SuiObjectInputArgument::SharedObject {
                id,
                initial_shared_version,
                mutable,
            } => write!(
                f,
                "SharedObject({}, version={}, mutable={})",
                id, initial_shared_version, mutable
            ),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InputArgument {
    Bool(bool),
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    U128(U128),
    U256(U256),
    Vector(MoveTypeTag, Vec<InputArgument>),
    Signer(MoveAddress),
    Address(MoveAddress),
    Object(MoveTypeTag, SuiObjectInputArgument), // TODO: Gated via `sui` feature
}

impl InputArgument {
    pub fn ty(&self) -> MoveTypeTag {
        match self {
            Self::Bool(_) => MoveTypeTag::Bool,
            Self::Address(_) => MoveTypeTag::Address,
            Self::Signer(_) => MoveTypeTag::Signer,
            Self::Vector(ty, _) => ty.clone(),
            Self::Object(ty, _) => ty.clone(),
            Self::U8(_) => MoveTypeTag::U8,
            Self::U16(_) => MoveTypeTag::U16,
            Self::U32(_) => MoveTypeTag::U32,
            Self::U64(_) => MoveTypeTag::U64,
            Self::U128(_) => MoveTypeTag::U128,
            Self::U256(_) => MoveTypeTag::U256,
        }
    }
}

impl Display for InputArgument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InputArgument::Bool(v) => write!(f, "Bool({})", v),
            InputArgument::U8(v) => write!(f, "U8({})", v),
            InputArgument::U16(v) => write!(f, "U16({})", v),
            InputArgument::U32(v) => write!(f, "U32({})", v),
            InputArgument::U64(v) => write!(f, "U64({})", v),
            InputArgument::U128(v) => write!(f, "U128({})", v),
            InputArgument::U256(v) => write!(f, "U256({})", v),
            InputArgument::Vector(ty, values) => {
                let elems = values
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "Vector<{}>[{}]", ty, elems)
            }
            InputArgument::Signer(addr) => write!(f, "Signer({})", addr),
            InputArgument::Address(addr) => write!(f, "Address({})", addr),
            InputArgument::Object(ty, obj) => write!(f, "Object<{}>({})", ty, obj),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MoveStructTag {
    pub address: MoveAddress,
    pub module: String,
    pub name: String,
    pub tys: Vec<MoveTypeTag>,
}

// Sui
impl From<MoveStructTag> for StructInput {
    fn from(value: MoveStructTag) -> Self {
        Self {
            address: value.address.into(),
            module: value.module,
            name: value.name,
            type_params: value.tys.into_iter().map(TypeInput::from).collect(),
        }
    }
}
impl TryFrom<MoveStructTag> for StructTag {
    type Error = MovyError;

    fn try_from(value: MoveStructTag) -> Result<Self, Self::Error> {
        Ok(Self {
            address: value.address.into(),
            module: Identifier::new(value.module)
                .map_err(|e| MovyError::InvalidIdentifier(e.to_string()))?,
            name: Identifier::new(value.name)
                .map_err(|e| MovyError::InvalidIdentifier(e.to_string()))?,
            type_params: value
                .tys
                .into_iter()
                .map(TypeTag::try_from)
                .collect::<Result<Vec<_>, _>>()?,
        })
    }
}

impl From<StructTag> for MoveStructTag {
    fn from(value: StructTag) -> Self {
        Self {
            address: value.address.into(),
            module: value.module.to_string(),
            name: value.name.to_string(),
            tys: value
                .type_params
                .into_iter()
                .map(MoveTypeTag::from)
                .collect(),
        }
    }
}

impl Display for MoveStructTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let type_params = if self.tys.is_empty() {
            String::new()
        } else {
            format!(
                "<{}>",
                self.tys
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };

        write!(
            f,
            "{}::{}::{}{}",
            self.address, self.module, self.name, type_params
        )
    }
}

impl FromStr for MoveStructTag {
    type Err = MovyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let struct_tag = StructTag::from_str(s).map_err(|e| {
            MovyError::InvalidIdentifier(format!("Invalid StructTag string: {}: {}", s, e))
        })?;
        Ok(MoveStructTag::from(struct_tag))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MoveTypeTag {
    Bool,
    U8,
    U16,
    U32,
    U64,
    U128,
    U256,
    Vector(Box<MoveTypeTag>),
    Address,
    Signer,
    Struct(MoveStructTag),
}

impl FromStr for MoveTypeTag {
    type Err = MovyError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        TypeTag::from_str(s).map_err(|e| e.into()).map(|v| v.into())
    }
}

impl MoveTypeTag {
    pub fn flat_addresses(&self) -> Vec<MoveAddress> {
        match self {
            MoveTypeTag::Struct(s) => {
                let mut sub_addrs = s
                    .tys
                    .iter()
                    .flat_map(|v| v.flat_addresses())
                    .collect::<Vec<_>>();
                sub_addrs.push(s.address);
                sub_addrs
            }
            MoveTypeTag::Vector(inner) => inner.flat_addresses(),
            _ => vec![],
        }
    }
    pub fn flat_structs(&self) -> Vec<MoveStructTag> {
        let mut out = vec![];
        match self {
            Self::Struct(st) => {
                out.push(st.clone());
                for ty in st.tys.iter() {
                    out.extend(ty.flat_structs().into_iter());
                }
            }
            Self::Vector(st) => {
                out.extend(st.flat_structs());
            }
            _ => {}
        }

        out
    }
}

impl From<MoveTypeTag> for TypeInput {
    fn from(value: MoveTypeTag) -> Self {
        match value {
            MoveTypeTag::Address => Self::Address,
            MoveTypeTag::Bool => Self::Bool,
            MoveTypeTag::Signer => Self::Signer,
            MoveTypeTag::U8 => Self::U8,
            MoveTypeTag::U16 => Self::U16,
            MoveTypeTag::U32 => Self::U32,
            MoveTypeTag::U128 => Self::U128,
            MoveTypeTag::U64 => Self::U64,
            MoveTypeTag::U256 => Self::U256,
            MoveTypeTag::Vector(v) => Self::Vector(Box::new(Self::from(*v))),
            MoveTypeTag::Struct(v) => Self::Struct(Box::new(StructInput::from(v))),
        }
    }
}

impl Display for MoveTypeTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MoveTypeTag::Bool => f.write_str("bool"),
            MoveTypeTag::U8 => f.write_str("u8"),
            MoveTypeTag::U16 => f.write_str("u16"),
            MoveTypeTag::U32 => f.write_str("u32"),
            MoveTypeTag::U64 => f.write_str("u64"),
            MoveTypeTag::U128 => f.write_str("u128"),
            MoveTypeTag::U256 => f.write_str("u256"),
            MoveTypeTag::Address => f.write_str("address"),
            MoveTypeTag::Signer => f.write_str("signer"),
            MoveTypeTag::Vector(inner) => write!(f, "vector<{}>", inner),
            MoveTypeTag::Struct(tag) => Display::fmt(tag, f),
        }
    }
}

impl From<TypeTag> for MoveTypeTag {
    fn from(value: TypeTag) -> Self {
        match value {
            TypeTag::Address => Self::Address,
            TypeTag::Bool => Self::Bool,
            TypeTag::Struct(v) => Self::Struct(MoveStructTag::from(*v)),
            TypeTag::Signer => Self::Signer,
            TypeTag::U8 => Self::U8,
            TypeTag::U16 => Self::U16,
            TypeTag::U32 => Self::U32,
            TypeTag::U128 => Self::U128,
            TypeTag::U64 => Self::U64,
            TypeTag::U256 => Self::U256,
            TypeTag::Vector(v) => Self::Vector(Box::new(Self::from(*v))),
        }
    }
}

impl TryFrom<MoveTypeTag> for TypeTag {
    type Error = MovyError;
    fn try_from(value: MoveTypeTag) -> Result<Self, Self::Error> {
        Ok(match value {
            MoveTypeTag::Address => Self::Address,
            MoveTypeTag::Bool => Self::Bool,
            MoveTypeTag::Signer => Self::Signer,
            MoveTypeTag::U8 => Self::U8,
            MoveTypeTag::U16 => Self::U16,
            MoveTypeTag::U32 => Self::U32,
            MoveTypeTag::U128 => Self::U128,
            MoveTypeTag::U64 => Self::U64,
            MoveTypeTag::U256 => Self::U256,
            MoveTypeTag::Vector(v) => Self::Vector(Box::new(Self::try_from(*v)?)),
            MoveTypeTag::Struct(v) => Self::Struct(Box::new(StructTag::try_from(v)?)),
        })
    }
}

#[derive(Copy, Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub enum SequenceArgument {
    GasCoin,
    Input(u16),
    Result(u16),
    NestedResult(u16, u16),
}

impl From<SequenceArgument> for Argument {
    fn from(value: SequenceArgument) -> Self {
        match value {
            SequenceArgument::GasCoin => Self::GasCoin,
            SequenceArgument::Input(v) => Self::Input(v),
            SequenceArgument::NestedResult(l, r) => Self::NestedResult(l, r),
            SequenceArgument::Result(v) => Self::Result(v),
        }
    }
}

impl Display for SequenceArgument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SequenceArgument::GasCoin => f.write_str("GasCoin"),
            SequenceArgument::Input(idx) => write!(f, "Input({})", idx),
            SequenceArgument::Result(idx) => write!(f, "Result({})", idx),
            SequenceArgument::NestedResult(m, n) => write!(f, "NestedResult({}, {})", m, n),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Hash)]
pub struct MoveCall {
    pub module_id: MoveAddress,
    pub module_name: String,
    pub function: String,
    pub type_arguments: Vec<MoveTypeTag>,
    pub arguments: Vec<SequenceArgument>,
}

impl MoveCall {
    fn fmt_with<F>(&self, mut fmt_arg: F) -> String
    where
        F: FnMut(&SequenceArgument) -> String,
    {
        let type_args = if self.type_arguments.is_empty() {
            String::new()
        } else {
            format!(
                "<{}>",
                self.type_arguments
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };

        let arguments = self
            .arguments
            .iter()
            .map(fmt_arg)
            .collect::<Vec<_>>()
            .join(", ");

        format!(
            "{}::{}::{}{}({})",
            self.module_id, self.module_name, self.function, type_args, arguments
        )
    }

    pub fn is_split(&self) -> bool {
        self.module_id == MoveAddress::two()
            && (self.module_name == "coin" || self.module_name == "balance")
            && self.function == "split"
    }
}

impl Display for MoveCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.fmt_with(|arg| arg.to_string()))
    }
}

impl From<MoveCall> for ProgrammableMoveCall {
    fn from(value: MoveCall) -> Self {
        Self {
            package: value.module_id.into(),
            module: value.module_name,
            function: value.function,
            type_arguments: value
                .type_arguments
                .into_iter()
                .map(TypeInput::from)
                .collect(),
            arguments: value.arguments.into_iter().map(Argument::from).collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Hash)]
pub enum MoveSequenceCall {
    // General Movecall
    Call(MoveCall),
    // Sui specific
    TransferObjects(Vec<SequenceArgument>, SequenceArgument),
    SplitCoins(SequenceArgument, Vec<SequenceArgument>),
    MergeCoins(SequenceArgument, Vec<SequenceArgument>),
    Publish(Vec<Vec<u8>>, Vec<MoveAddress>),
    MakeMoveVec(MoveTypeTag, Vec<SequenceArgument>),
    Upgrade(
        Vec<Vec<u8>>,
        Vec<MoveAddress>,
        MoveAddress,
        SequenceArgument,
    ),
}

impl MoveSequenceCall {
    fn fmt_with<F>(&self, mut fmt_arg: F) -> String
    where
        F: FnMut(&SequenceArgument) -> String,
    {
        match self {
            MoveSequenceCall::Call(mc) => format!("MoveCall({})", mc.fmt_with(fmt_arg)),
            MoveSequenceCall::TransferObjects(args, dst) => {
                let srcs = args.iter().map(&mut fmt_arg).collect::<Vec<_>>().join(", ");
                let dst = fmt_arg(dst);

                format!("TransferObjects([{}], {})", srcs, dst)
            }
            MoveSequenceCall::SplitCoins(src, amounts) => {
                let src = fmt_arg(src);
                let amounts = amounts
                    .iter()
                    .map(&mut fmt_arg)
                    .collect::<Vec<_>>()
                    .join(", ");

                format!("SplitCoins({}, [{}])", src, amounts)
            }
            MoveSequenceCall::MergeCoins(dst, srcs) => {
                let dst = fmt_arg(dst);
                let srcs = srcs.iter().map(&mut fmt_arg).collect::<Vec<_>>().join(", ");

                format!("MergeCoins({}, [{}])", dst, srcs)
            }
            MoveSequenceCall::Publish(modules, deps) => {
                let deps = deps
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("Publish({} modules, [{}])", modules.len(), deps)
            }
            MoveSequenceCall::MakeMoveVec(ty, args) => {
                let args = args.iter().map(&mut fmt_arg).collect::<Vec<_>>().join(", ");
                format!("MakeMoveVec({}, [{}])", ty, args)
            }
            MoveSequenceCall::Upgrade(modules, deps, package, ticket) => {
                let deps = deps
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                let ticket = fmt_arg(ticket);
                format!(
                    "Upgrade({} modules, [{}], {}, {})",
                    modules.len(),
                    deps,
                    package,
                    ticket
                )
            }
        }
    }
}

impl Display for MoveSequenceCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.fmt_with(|arg| arg.to_string()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Hash)]
pub struct MoveSequence {
    pub inputs: Vec<InputArgument>,
    pub commands: Vec<MoveSequenceCall>,
}

impl MoveSequence {
    fn sui_builder_input_arg(
        builder: &mut ProgrammableTransactionBuilder,
        arg: &InputArgument,
    ) -> Result<Argument, MovyError> {
        let v = match arg {
            InputArgument::Bool(v) => builder.pure(*v),
            InputArgument::U8(v) => builder.pure(*v),
            InputArgument::U16(v) => builder.pure(*v),
            InputArgument::U32(v) => builder.pure(*v),
            InputArgument::U64(v) => builder.pure(*v),
            InputArgument::U128(v) => builder.pure(*v),
            InputArgument::U256(v) => {
                let v = move_core_types::u256::U256::from_le_bytes(&v.to_le_bytes());
                builder.pure(v)
            }
            InputArgument::Address(v) => {
                let v = SuiAddress::from(*v);
                builder.pure(v)
            }
            InputArgument::Signer(v) => {
                let v = SuiAddress::from(*v);
                builder.pure(v)
            }
            InputArgument::Vector(ty, vs) => {
                let args = vs
                    .iter()
                    .map(|v| Self::sui_builder_input_arg(builder, v))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(builder.command(Command::MakeMoveVec(Some(ty.clone().into()), args)))
            }
            InputArgument::Object(_, v) => builder.obj(v.clone().into()),
        };
        Ok(v?)
    }
    pub fn to_ptb(&self) -> Result<ProgrammableTransaction, MovyError> {
        let mut builder = ProgrammableTransactionBuilder::new();

        for input in self.inputs.iter() {
            Self::sui_builder_input_arg(&mut builder, input)?;
        }

        for cmd in self.commands.iter() {
            match cmd {
                MoveSequenceCall::Call(call) => {
                    builder.command(Command::MoveCall(Box::new(call.clone().into())));
                }
                MoveSequenceCall::MakeMoveVec(ty, args) => {
                    builder.command(Command::MakeMoveVec(
                        Some(ty.clone().into()),
                        args.iter().map(|v| (*v).into()).collect(),
                    ));
                }
                MoveSequenceCall::Publish(modules, address) => {
                    builder.command(Command::Publish(
                        modules.clone(),
                        address.iter().map(|v| (*v).into()).collect(),
                    ));
                }
                MoveSequenceCall::TransferObjects(args, dst) => {
                    let src = args.iter().map(|v| (*v).into()).collect();
                    builder.command(Command::TransferObjects(src, (*dst).into()));
                }
                MoveSequenceCall::MergeCoins(dst, src) => {
                    let src = src.iter().map(|v| (*v).into()).collect();
                    builder.command(Command::MergeCoins((*dst).into(), src));
                }
                MoveSequenceCall::SplitCoins(src, amounts) => {
                    let amounts = amounts.iter().map(|v| (*v).into()).collect();
                    builder.command(Command::SplitCoins((*src).into(), amounts));
                }
                MoveSequenceCall::Upgrade(modules, deps, package, ticket) => {
                    builder.command(Command::Upgrade(
                        modules.clone(),
                        deps.iter().map(|v| (*v).into()).collect(),
                        (*package).into(),
                        (*ticket).into(),
                    ));
                }
            }
        }

        Ok(builder.finish())
    }
}

impl Display for MoveSequence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let formatted_inputs = self
            .inputs
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();

        let fmt_arg = |arg: &SequenceArgument| match arg {
            SequenceArgument::Input(idx) => formatted_inputs
                .get(*idx as usize)
                .map(|v| format!("Input({}, {})", idx, v))
                .unwrap_or_else(|| format!("Input({})", idx)),
            _ => arg.to_string(),
        };

        let calls = self
            .commands
            .iter()
            .map(|call| call.fmt_with(&fmt_arg))
            .collect::<Vec<_>>();

        let inputs_display = if formatted_inputs.is_empty() {
            "<none>".to_string()
        } else {
            formatted_inputs.join("\n\t")
        };

        let commands_display = if calls.is_empty() {
            "<none>".to_string()
        } else {
            calls.join("\n\t")
        };

        write!(
            f,
            "Inputs:\n\t{}\nCommands:\n\t{}",
            inputs_display, commands_display
        )
    }
}
