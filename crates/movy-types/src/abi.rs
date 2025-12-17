use std::{collections::BTreeMap, fmt::Display, str::FromStr};

use alloy_primitives::{U128, U256};
use color_eyre::eyre::eyre;
use itertools::Itertools;
use move_binary_format::{
    CompiledModule,
    file_format::{
        AbilitySet, DatatypeHandleIndex, DatatypeTyParameter, FunctionDefinition, FunctionHandle,
        ModuleHandle, SignatureToken, Visibility,
    },
};
use serde::{Deserialize, Serialize};
use sui_types::{base_types::ObjectID, object::Object};

use crate::{
    error::MovyError,
    input::{InputArgument, MoveAddress, MoveStructTag, MoveTypeTag},
};

pub const MOVY_INIT: &str = "movy_init";
pub const MOVY_ORACLE: &str = "movy_oracle";
pub const MOVY_PRE: &str = "movy_pre";
pub const MOVY_POST: &str = "movy_post";
pub const MOVY_SEQUENCE: &str = "ptb";

bitflags::bitflags! {
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord, Copy)]
    pub struct MoveAbility: u8 {
        const COPY  = 0b00000001;
        const DROP  = 0b00000010;
        const STORE = 0b00000100;
        const KEY   = 0b00001000;
        const PRIMITIVES = 0b00000111;
    }
}

impl Display for MoveAbility {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut abilities = vec![];
        if self.contains(Self::DROP) {
            abilities.push("drop");
        }

        if self.contains(Self::STORE) {
            abilities.push("store");
        }

        if self.contains(Self::KEY) {
            abilities.push("key");
        }

        if self.contains(Self::COPY) {
            abilities.push("copy");
        }

        f.write_str(&abilities.into_iter().join(" + "))
    }
}

impl MoveAbility {
    pub fn to_string_with_name(&self, name: &str) -> String {
        if self.is_empty() {
            name.to_string()
        } else {
            format!("{}: {}", name, self)
        }
    }
    pub fn is_hot_potato(&self) -> bool {
        !self.contains(Self::DROP) && !self.contains(Self::STORE)
    }
    pub fn is_subset_of(&self, other: &MoveAbility) -> bool {
        (self.bits() & other.bits()) == self.bits()
    }
}

impl From<AbilitySet> for MoveAbility {
    fn from(value: AbilitySet) -> Self {
        let mut ability = Self::empty();
        if value.has_copy() {
            ability |= Self::DROP;
        }
        if value.has_key() {
            ability |= Self::KEY;
        }
        if value.has_store() {
            ability |= Self::STORE;
        }
        if value.has_drop() {
            ability |= Self::DROP;
        }
        ability
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MoveStructTypeParameters {
    pub constraints: MoveAbility,
    pub phantom: bool,
}

impl MoveStructTypeParameters {
    pub fn to_string_with_name(&self, name: &str) -> String {
        format!(
            "{}{}",
            if self.phantom { "phantom " } else { "" },
            self.constraints.to_string_with_name(name)
        )
    }
}

impl From<DatatypeTyParameter> for MoveStructTypeParameters {
    fn from(value: DatatypeTyParameter) -> Self {
        Self {
            constraints: value.constraints.into(),
            phantom: value.is_phantom,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MoveStructAbi {
    pub module_id: MoveModuleId,
    pub struct_name: String,
    pub abilities: MoveAbility,
    pub type_parameters: Vec<MoveStructTypeParameters>,
}

impl MoveStructAbi {
    pub fn from_module_idx(idx: DatatypeHandleIndex, module: &CompiledModule) -> Self {
        let dty = module.datatype_handle_at(idx);
        let sname = module.identifier_at(dty.name).to_string();
        let ability = MoveAbility::from(dty.abilities);
        let struct_module = module.module_handle_at(dty.module);
        let module_id = module.address_identifier_at(struct_module.address);
        let module_name = module.identifier_at(struct_module.name).to_string();
        let tys = dty
            .type_parameters
            .iter()
            .map(|v| MoveStructTypeParameters::from(*v))
            .collect();
        Self {
            module_id: MoveModuleId {
                module_address: (*module_id).into(),
                module_name,
            },
            struct_name: sname,
            abilities: ability,
            type_parameters: tys,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MoveAbiSignatureToken {
    Bool,
    U8,
    U16,
    U32,
    U64,
    U128,
    U256,
    Address,
    Signer,
    Vector(Box<MoveAbiSignatureToken>),
    Struct(MoveStructAbi),
    StructInstantiation(MoveStructAbi, Vec<MoveAbiSignatureToken>),
    TypeParameter(u16, MoveAbility),
    Reference(Box<MoveAbiSignatureToken>),
    MutableReference(Box<MoveAbiSignatureToken>),
    // Adopted by Aptos
    // I8,
    // I16,
    // I32,
    // I64,
    // I128,
    // I256,
    // Aptos even support functions...
}

impl Display for MoveAbiSignatureToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bool => f.write_str("bool"),
            Self::U8 => f.write_str("u8"),
            Self::U16 => f.write_str("u16"),
            Self::U32 => f.write_str("u32"),
            Self::U64 => f.write_str("u64"),
            Self::U128 => f.write_str("u128"),
            Self::U256 => f.write_str("u256"),
            Self::Address => f.write_str("address"),
            Self::Signer => f.write_str("signer"),
            Self::Vector(v) => f.write_fmt(format_args!(
                "vector<{}>",
                if f.alternate() {
                    format!("{:#}", v)
                } else {
                    v.to_string()
                }
            )),
            Self::Struct(s) => {
                let ty_string = if s.type_parameters.is_empty() {
                    "".to_string()
                } else {
                    let tys = s
                        .type_parameters
                        .iter()
                        .enumerate()
                        .map(|(idx, v)| v.to_string_with_name(&format!("ST{}", idx)))
                        .join(", ");
                    format!("<{}>", tys)
                };
                f.write_fmt(format_args!(
                    "{}:{}:{}{}",
                    if f.alternate() {
                        s.module_id.module_address.short()
                    } else {
                        s.module_id.module_address.to_string()
                    },
                    &s.module_id.module_name,
                    &s.struct_name,
                    ty_string
                ))
            }
            Self::StructInstantiation(s, insts) => {
                let ty_string = if s.type_parameters.is_empty() {
                    "".to_string()
                } else {
                    format!(
                        "<{}>",
                        insts
                            .iter()
                            .map(|v| if f.alternate() {
                                format!("{:#}", v)
                            } else {
                                v.to_string()
                            })
                            .join(", ")
                    )
                };
                f.write_fmt(format_args!(
                    "{}:{}:{}{}",
                    if f.alternate() {
                        s.module_id.module_address.short()
                    } else {
                        s.module_id.module_address.to_string()
                    },
                    &s.module_id.module_name,
                    &s.struct_name,
                    ty_string
                ))
            }
            Self::MutableReference(v) => f.write_fmt(format_args!(
                "&mut {}",
                if f.alternate() {
                    format!("{:#}", v)
                } else {
                    v.to_string()
                }
            )),
            Self::Reference(v) => f.write_fmt(format_args!(
                "&{}",
                if f.alternate() {
                    format!("{:#}", v)
                } else {
                    v.to_string()
                }
            )),
            Self::TypeParameter(idx, _) => f.write_fmt(format_args!("T{}", idx)),
        }
    }
}

impl MoveAbiSignatureToken {
    pub fn published_at(&mut self, previous: MoveAddress, address: MoveAddress) {
        match self {
            Self::Struct(st) => {
                if st.module_id.module_address == previous {
                    st.module_id.module_address = address;
                }
            }
            Self::StructInstantiation(st, insts) => {
                if st.module_id.module_address == previous {
                    st.module_id.module_address = address;
                }
                for inst in insts.iter_mut() {
                    inst.published_at(previous, address);
                }
            }
            Self::MutableReference(mt) => mt.published_at(previous, address),
            Self::Reference(rf) => rf.published_at(previous, address),
            Self::Vector(v) => v.published_at(previous, address),
            _ => {}
        }
    }
    pub fn dereference(&self) -> Option<&Box<Self>> {
        match self {
            Self::Reference(v) => Some(v),
            Self::MutableReference(v) => Some(v),
            _ => None,
        }
    }
    pub fn to_type_tag(&self) -> Option<MoveTypeTag> {
        match self {
            Self::Bool => Some(MoveTypeTag::Bool),
            Self::U8 => Some(MoveTypeTag::U8),
            Self::U16 => Some(MoveTypeTag::U16),
            Self::U32 => Some(MoveTypeTag::U32),
            Self::U64 => Some(MoveTypeTag::U64),
            Self::U128 => Some(MoveTypeTag::U128),
            Self::U256 => Some(MoveTypeTag::U256),
            Self::Address => Some(MoveTypeTag::Address),
            Self::Signer => Some(MoveTypeTag::Signer),
            Self::Vector(v) => Some(MoveTypeTag::Vector(Box::new(v.to_type_tag()?))),
            Self::StructInstantiation(st, tys) => {
                if st.type_parameters.len() == tys.len() {
                    let mut insts = vec![];
                    for ty in tys.iter() {
                        let inst = ty.to_type_tag()?;
                        insts.push(inst);
                    }
                    Some(MoveTypeTag::Struct(MoveStructTag {
                        address: st.module_id.module_address,
                        module: st.module_id.module_name.clone(),
                        name: st.struct_name.clone(),
                        tys: insts,
                    }))
                } else {
                    None
                }
            }
            _ => None,
        }
    }
    pub fn from_sui_token_module(
        value: &SignatureToken,
        tys: &Vec<MoveAbility>,
        module: &CompiledModule,
    ) -> Self {
        match value {
            SignatureToken::Address => Self::Address,
            SignatureToken::Bool => Self::Bool,
            SignatureToken::Signer => Self::Signer,
            SignatureToken::U8 => Self::U8,
            SignatureToken::U16 => Self::U16,
            SignatureToken::U32 => Self::U32,
            SignatureToken::U64 => Self::U64,
            SignatureToken::U128 => Self::U128,
            SignatureToken::U256 => Self::U256,
            SignatureToken::Vector(v) => {
                Self::Vector(Box::new(Self::from_sui_token_module(v, tys, module)))
            }
            SignatureToken::Datatype(dty) => {
                let st = MoveStructAbi::from_module_idx(*dty, module);
                Self::Struct(st)
            }
            SignatureToken::DatatypeInstantiation(v) => {
                let (dty, insts) = *(v.clone());
                let st = MoveStructAbi::from_module_idx(dty, module);
                let insts = insts
                    .into_iter()
                    .map(|v| Self::from_sui_token_module(&v, tys, module))
                    .collect();
                Self::StructInstantiation(st, insts)
            }
            SignatureToken::TypeParameter(ty) => {
                let ability = tys.get(*ty as usize).unwrap();
                Self::TypeParameter(*ty, *ability)
            }
            SignatureToken::Reference(r) => {
                Self::Reference(Box::new(Self::from_sui_token_module(r, tys, module)))
            }
            SignatureToken::MutableReference(r) => {
                Self::MutableReference(Box::new(Self::from_sui_token_module(r, tys, module)))
            }
        }
    }

    pub fn is_mutable(&self) -> bool {
        match self {
            MoveAbiSignatureToken::Bool
            | MoveAbiSignatureToken::Address
            | MoveAbiSignatureToken::U8
            | MoveAbiSignatureToken::U16
            | MoveAbiSignatureToken::U32
            | MoveAbiSignatureToken::U64
            | MoveAbiSignatureToken::U128
            | MoveAbiSignatureToken::U256 => true,
            MoveAbiSignatureToken::Vector(inner) => inner.is_mutable(),
            _ => false,
        }
    }

    pub fn is_tx_context(&self) -> bool {
        match self {
            MoveAbiSignatureToken::Struct(inner) => {
                inner.module_id.module_address == MoveAddress::two()
                    && inner.module_id.module_name == "tx_context"
                    && inner.struct_name == "TxContext"
            }
            MoveAbiSignatureToken::Reference(inner) => inner.is_tx_context(),
            MoveAbiSignatureToken::MutableReference(inner) => inner.is_tx_context(),
            _ => false,
        }
    }

    pub fn is_hot_potato(&self) -> bool {
        match self {
            MoveAbiSignatureToken::Struct(inner)
            | MoveAbiSignatureToken::StructInstantiation(inner, _) => {
                if self.is_balance() || self.is_tx_context() {
                    return false;
                }
                inner.abilities.is_hot_potato()
            }
            MoveAbiSignatureToken::Vector(inner) => inner.is_hot_potato(),
            _ => false,
        }
    }

    pub fn is_balance(&self) -> bool {
        let MoveAbiSignatureToken::Struct(inner) = self else {
            return false;
        };
        inner.module_id.module_address == MoveAddress::two()
            && inner.module_id.module_name == "balance"
            && inner.struct_name == "Balance"
    }

    pub fn is_coin(&self) -> bool {
        let MoveAbiSignatureToken::Struct(inner) = self else {
            return false;
        };
        inner.module_id.module_address == MoveAddress::two()
            && inner.module_id.module_name == "coin"
            && inner.struct_name == "Coin"
    }

    // A, &A, &mut A, Vec<A>, T, &T, &mut T need sample
    pub fn needs_sample(&self) -> bool {
        if self.is_tx_context() {
            return false;
        }
        match self {
            MoveAbiSignatureToken::Struct { .. } => true,
            MoveAbiSignatureToken::StructInstantiation(_, _) => true,
            MoveAbiSignatureToken::Reference(b)
            | MoveAbiSignatureToken::MutableReference(b)
            | MoveAbiSignatureToken::Vector(b) => match b.as_ref() {
                MoveAbiSignatureToken::Struct { .. } => true,
                MoveAbiSignatureToken::StructInstantiation(_, _) => true,
                MoveAbiSignatureToken::TypeParameter(_, _) => true,
                _ => b.needs_sample(),
            },
            MoveAbiSignatureToken::TypeParameter(_, _) => true,
            _ => false,
        }
    }

    pub fn has_copy(&self) -> bool {
        matches!(self, MoveAbiSignatureToken::Struct(inner) | MoveAbiSignatureToken::StructInstantiation(inner, _) if inner.abilities.contains(MoveAbility::COPY))
    }

    pub fn is_key_store(&self) -> bool {
        matches!(self, MoveAbiSignatureToken::Struct(inner) | MoveAbiSignatureToken::StructInstantiation(inner, _) if inner.abilities.contains(MoveAbility::KEY | MoveAbility::STORE))
    }

    pub fn gen_input_arg(&self) -> Option<InputArgument> {
        match self {
            MoveAbiSignatureToken::Bool => Some(InputArgument::Bool(false)),
            MoveAbiSignatureToken::Address => Some(InputArgument::Address(MoveAddress::zero())),
            MoveAbiSignatureToken::U8 => Some(InputArgument::U8(0)),
            MoveAbiSignatureToken::U16 => Some(InputArgument::U16(0)),
            MoveAbiSignatureToken::U32 => Some(InputArgument::U32(0)),
            MoveAbiSignatureToken::U64 => Some(InputArgument::U64(0)),
            MoveAbiSignatureToken::U128 => Some(InputArgument::U128(U128::ZERO)),
            MoveAbiSignatureToken::U256 => Some(InputArgument::U256(U256::ZERO)),
            MoveAbiSignatureToken::Signer => Some(InputArgument::Signer(MoveAddress::zero())),
            MoveAbiSignatureToken::Vector(inner) => inner.gen_input_arg().map(|v| {
                InputArgument::Vector(
                    inner.to_type_tag().expect("Vector inner type tag"),
                    vec![v; 1024],
                )
            }),
            MoveAbiSignatureToken::Reference(inner)
            | MoveAbiSignatureToken::MutableReference(inner) => {
                let inner_value = inner.gen_input_arg();
                if !inner.needs_sample() {
                    inner_value
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    pub fn ability(&self) -> Option<MoveAbility> {
        match self {
            MoveAbiSignatureToken::Bool => Some(MoveAbility::PRIMITIVES),
            MoveAbiSignatureToken::Address => Some(MoveAbility::PRIMITIVES),
            MoveAbiSignatureToken::U8 => Some(MoveAbility::PRIMITIVES),
            MoveAbiSignatureToken::U16 => Some(MoveAbility::PRIMITIVES),
            MoveAbiSignatureToken::U32 => Some(MoveAbility::PRIMITIVES),
            MoveAbiSignatureToken::U64 => Some(MoveAbility::PRIMITIVES),
            MoveAbiSignatureToken::U128 => Some(MoveAbility::PRIMITIVES),
            MoveAbiSignatureToken::U256 => Some(MoveAbility::PRIMITIVES),
            MoveAbiSignatureToken::Signer => Some(MoveAbility::DROP),
            MoveAbiSignatureToken::Vector(_) => Some(MoveAbility::PRIMITIVES),
            MoveAbiSignatureToken::Struct(inner)
            | MoveAbiSignatureToken::StructInstantiation(inner, _) => Some(inner.abilities),
            _ => None,
        }
    }

    // return a map of type parameter index to instantiated type tag
    pub fn extract_ty_args(
        &self,
        instantiated_ty: &MoveTypeTag,
    ) -> Option<BTreeMap<u16, MoveTypeTag>> {
        let mut ty_args = BTreeMap::new();
        match (self, instantiated_ty) {
            (MoveAbiSignatureToken::Vector(inner), MoveTypeTag::Vector(inner_ty)) => {
                return inner.extract_ty_args(inner_ty);
            }
            (
                MoveAbiSignatureToken::StructInstantiation(inner, type_args),
                MoveTypeTag::Struct(tag),
            ) => {
                if inner.module_id.module_address != tag.address
                    || inner.module_id.module_name != tag.module
                    || inner.struct_name != tag.name
                {
                    return None;
                }
                for (sub_generic_ty, sub_instantiated_ty) in type_args.iter().zip(tag.tys.iter()) {
                    if let Some(sub_ty_args) = sub_generic_ty.extract_ty_args(sub_instantiated_ty) {
                        for (index, sub_ty_arg) in sub_ty_args {
                            if ty_args.contains_key(&index) && ty_args[&index] != sub_ty_arg {
                                // If the type argument is already set and does not match, return None
                                return None;
                            }
                            ty_args.insert(index, sub_ty_arg);
                        }
                    } else {
                        return None;
                    }
                }
            }
            (MoveAbiSignatureToken::Struct(inner), MoveTypeTag::Struct(tag)) => {
                if inner.module_id.module_address != tag.address
                    || inner.module_id.module_name != tag.module
                    || inner.struct_name != tag.name
                {
                    return None;
                }
                if !inner.type_parameters.is_empty() {
                    // Generic struct without instantiation cannot extract type arguments
                    return None;
                }
            }
            (MoveAbiSignatureToken::Reference(inner), _) => {
                return inner.extract_ty_args(instantiated_ty);
            }
            (MoveAbiSignatureToken::MutableReference(inner), _) => {
                return inner.extract_ty_args(instantiated_ty);
            }
            (MoveAbiSignatureToken::TypeParameter(index, _), _) => {
                ty_args.insert(*index, instantiated_ty.clone());
            }
            (MoveAbiSignatureToken::Bool, MoveTypeTag::Bool)
            | (MoveAbiSignatureToken::Address, MoveTypeTag::Address)
            | (MoveAbiSignatureToken::U8, MoveTypeTag::U8)
            | (MoveAbiSignatureToken::U16, MoveTypeTag::U16)
            | (MoveAbiSignatureToken::U32, MoveTypeTag::U32)
            | (MoveAbiSignatureToken::U64, MoveTypeTag::U64)
            | (MoveAbiSignatureToken::U128, MoveTypeTag::U128)
            | (MoveAbiSignatureToken::U256, MoveTypeTag::U256)
            | (MoveAbiSignatureToken::Signer, MoveTypeTag::Signer) => {
                // No type arguments needed for these types
            }
            _ => return None,
        }
        Some(ty_args)
    }

    pub fn partial_extract_ty_args(
        &self,
        other: &MoveAbiSignatureToken,
    ) -> Option<(
        BTreeMap<u16, MoveTypeTag>,
        BTreeMap<u16, MoveTypeTag>,
        Vec<(u16, u16)>,
    )> {
        let mut ty_args = BTreeMap::new();
        let mut other_ty_args = BTreeMap::new();
        let mut mapping = Vec::new();
        match (self, other) {
            (
                MoveAbiSignatureToken::TypeParameter(index, _),
                MoveAbiSignatureToken::TypeParameter(other_index, _),
            ) => {
                mapping.push((*index, *other_index));
            }
            (MoveAbiSignatureToken::TypeParameter(index, _), _) => {
                // ignore nested type parameters case
                ty_args.insert(*index, other.subst(&BTreeMap::new())?);
            }
            (_, MoveAbiSignatureToken::TypeParameter(other_index, _)) => {
                // ignore nested type parameters case
                other_ty_args.insert(*other_index, self.subst(&BTreeMap::new())?);
            }
            (MoveAbiSignatureToken::Struct(inner), MoveAbiSignatureToken::Struct(other_inner)) => {
                if inner.module_id != other_inner.module_id
                    || inner.struct_name != other_inner.struct_name
                    || inner.type_parameters.len() != other_inner.type_parameters.len()
                {
                    return None;
                }
            }
            (
                MoveAbiSignatureToken::StructInstantiation(inner, type_args),
                MoveAbiSignatureToken::StructInstantiation(other_inner, other_type_args),
            ) => {
                if inner.module_id != other_inner.module_id
                    || inner.struct_name != other_inner.struct_name
                    || type_args.len() != other_type_args.len()
                {
                    return None;
                }
                for (sub_generic_ty, sub_other_ty) in type_args.iter().zip(other_type_args.iter()) {
                    let (sub_ty_args, sub_other_ty_args, sub_mapping) =
                        sub_generic_ty.partial_extract_ty_args(sub_other_ty)?;
                    for (index, ty) in sub_ty_args {
                        if ty_args.contains_key(&index) && ty_args[&index] != ty {
                            // If the type argument is already set and does not match, return None
                            return None;
                        }
                        ty_args.insert(index, ty);
                    }
                    for (index, ty) in sub_other_ty_args {
                        if other_ty_args.contains_key(&index) && other_ty_args[&index] != ty {
                            // If the type argument is already set and does not match, return None
                            return None;
                        }
                        other_ty_args.insert(index, ty);
                    }
                    mapping.extend(sub_mapping);
                }
            }
            (
                MoveAbiSignatureToken::MutableReference(inner),
                MoveAbiSignatureToken::MutableReference(other_inner),
            )
            | (
                MoveAbiSignatureToken::Reference(inner),
                MoveAbiSignatureToken::MutableReference(other_inner),
            )
            | (
                MoveAbiSignatureToken::Reference(inner),
                MoveAbiSignatureToken::Reference(other_inner),
            )
            | (MoveAbiSignatureToken::Vector(inner), MoveAbiSignatureToken::Vector(other_inner)) => {
                // other indicates producer type, self indicates consumer type
                // e.g., &mut T can be passed in &T, &mut T, but not T
                return inner.partial_extract_ty_args(other_inner);
            }
            (MoveAbiSignatureToken::MutableReference(inner), _) => {
                return inner.partial_extract_ty_args(other);
            }
            (MoveAbiSignatureToken::Reference(inner), _) => {
                return inner.partial_extract_ty_args(other);
            }
            _ => {
                if self != other {
                    return None;
                }
            }
        }
        Some((ty_args, other_ty_args, mapping))
    }

    pub fn subst(&self, ty_args: &BTreeMap<u16, MoveTypeTag>) -> Option<MoveTypeTag> {
        match self {
            MoveAbiSignatureToken::Vector(inner) => inner
                .subst(ty_args)
                .map(|inner_ty| MoveTypeTag::Vector(Box::new(inner_ty))),
            MoveAbiSignatureToken::StructInstantiation(inner, type_params) => {
                let tys = type_params
                    .iter()
                    .map(|ty| ty.subst(ty_args))
                    .collect::<Option<Vec<_>>>()?;
                Some(MoveTypeTag::Struct(MoveStructTag {
                    address: inner.module_id.module_address,
                    module: inner.module_id.module_name.clone(),
                    name: inner.struct_name.clone(),
                    tys,
                }))
            }
            MoveAbiSignatureToken::Struct(inner) => {
                if inner.type_parameters.is_empty() {
                    Some(MoveTypeTag::Struct(MoveStructTag {
                        address: inner.module_id.module_address,
                        module: inner.module_id.module_name.clone(),
                        name: inner.struct_name.clone(),
                        tys: vec![],
                    }))
                } else {
                    None
                }
            }
            MoveAbiSignatureToken::Reference(inner) => inner.subst(ty_args),
            MoveAbiSignatureToken::MutableReference(inner) => inner.subst(ty_args),
            MoveAbiSignatureToken::TypeParameter(index, _) => ty_args.get(index).cloned(),
            MoveAbiSignatureToken::Bool => Some(MoveTypeTag::Bool),
            MoveAbiSignatureToken::Address => Some(MoveTypeTag::Address),
            MoveAbiSignatureToken::U8 => Some(MoveTypeTag::U8),
            MoveAbiSignatureToken::U16 => Some(MoveTypeTag::U16),
            MoveAbiSignatureToken::U32 => Some(MoveTypeTag::U32),
            MoveAbiSignatureToken::U64 => Some(MoveTypeTag::U64),
            MoveAbiSignatureToken::U128 => Some(MoveTypeTag::U128),
            MoveAbiSignatureToken::U256 => Some(MoveTypeTag::U256),
            MoveAbiSignatureToken::Signer => Some(MoveTypeTag::Signer),
        }
    }

    pub fn partial_subst(&self, ty_args: &BTreeMap<u16, MoveTypeTag>) -> MoveAbiSignatureToken {
        match self {
            MoveAbiSignatureToken::Vector(inner) => {
                MoveAbiSignatureToken::Vector(Box::new(inner.partial_subst(ty_args)))
            }
            MoveAbiSignatureToken::StructInstantiation(inner, type_params) => {
                let type_arguments = type_params
                    .iter()
                    .map(|ty| ty.partial_subst(ty_args))
                    .collect::<Vec<_>>();
                MoveAbiSignatureToken::StructInstantiation(inner.clone(), type_arguments)
            }
            MoveAbiSignatureToken::Reference(inner) => {
                MoveAbiSignatureToken::Reference(Box::new(inner.partial_subst(ty_args)))
            }
            MoveAbiSignatureToken::MutableReference(inner) => {
                MoveAbiSignatureToken::MutableReference(Box::new(inner.partial_subst(ty_args)))
            }
            MoveAbiSignatureToken::TypeParameter(index, _) => ty_args
                .get(index)
                .cloned()
                .map(|ty_tag| MoveAbiSignatureToken::from_type_tag(&ty_tag))
                .unwrap_or(self.clone()),
            _ => self.clone(),
        }
    }

    pub fn from_type_tag(ty: &MoveTypeTag) -> Self
    where
        Self: Sized,
    {
        match ty {
            MoveTypeTag::Bool => MoveAbiSignatureToken::Bool,
            MoveTypeTag::Address => MoveAbiSignatureToken::Address,
            MoveTypeTag::U8 => MoveAbiSignatureToken::U8,
            MoveTypeTag::U16 => MoveAbiSignatureToken::U16,
            MoveTypeTag::U32 => MoveAbiSignatureToken::U32,
            MoveTypeTag::U64 => MoveAbiSignatureToken::U64,
            MoveTypeTag::U128 => MoveAbiSignatureToken::U128,
            MoveTypeTag::U256 => MoveAbiSignatureToken::U256,
            MoveTypeTag::Signer => MoveAbiSignatureToken::Signer,
            MoveTypeTag::Vector(inner) => {
                MoveAbiSignatureToken::Vector(Box::new(MoveAbiSignatureToken::from_type_tag(inner)))
            }
            MoveTypeTag::Struct(tag) => {
                if tag.tys.is_empty() {
                    MoveAbiSignatureToken::Struct(MoveStructAbi {
                        module_id: MoveModuleId {
                            module_address: tag.address,
                            module_name: tag.module.clone(),
                        },
                        struct_name: tag.name.clone(),
                        abilities: MoveAbility::empty(), // Abilities are not known from TypeTag
                        type_parameters: vec![], // Type parameters are not known from TypeTag
                    })
                } else {
                    let type_arguments = tag
                        .tys
                        .iter()
                        .map(MoveAbiSignatureToken::from_type_tag)
                        .collect();
                    MoveAbiSignatureToken::StructInstantiation(
                        MoveStructAbi {
                            module_id: MoveModuleId {
                                module_address: tag.address,
                                module_name: tag.module.clone(),
                            },
                            struct_name: tag.name.clone(),
                            abilities: MoveAbility::empty(), // Abilities are not known from TypeTag
                            type_parameters: vec![], // Type parameters are not known from TypeTag
                        },
                        type_arguments,
                    )
                }
            }
        }
    }

    pub fn contains_type_param(&self, index: u16) -> bool {
        match self {
            MoveAbiSignatureToken::TypeParameter(i, _) => *i == index,
            MoveAbiSignatureToken::Vector(inner) => inner.contains_type_param(index),
            MoveAbiSignatureToken::Reference(inner)
            | MoveAbiSignatureToken::MutableReference(inner) => inner.contains_type_param(index),
            MoveAbiSignatureToken::StructInstantiation(_, type_params) => {
                type_params.iter().any(|ty| ty.contains_type_param(index))
            }
            _ => false,
        }
    }
}

impl From<MoveTypeTag> for MoveAbiSignatureToken {
    fn from(value: MoveTypeTag) -> Self {
        Self::from_type_tag(&value)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MoveFunctionVisibility {
    Public,
    Private,
    Friend,
}

impl Display for MoveFunctionVisibility {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Private => f.write_str("private"),
            Self::Public => f.write_str("public"),
            Self::Friend => f.write_str("friend"),
        }
    }
}

impl From<Visibility> for MoveFunctionVisibility {
    fn from(value: Visibility) -> Self {
        match value {
            Visibility::Friend => Self::Friend,
            Visibility::Public => Self::Public,
            Visibility::Private => Self::Private,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MoveFunctionAbi {
    pub name: String,
    pub parameters: Vec<MoveAbiSignatureToken>,
    pub return_paramters: Vec<MoveAbiSignatureToken>,
    pub type_parameters: Vec<MoveAbility>,
    pub visibility: MoveFunctionVisibility,
    // TODO: Aptos's acquires
}

impl MoveFunctionAbi {
    pub fn is_movy_init(&self) -> bool {
        self.name == MOVY_INIT
    }

    pub fn is_movy_oracle(&self) -> bool {
        self.name.starts_with(MOVY_ORACLE)
    }

    pub fn try_derive_movy_pre(&self) -> Option<&str> {
        if self.name.starts_with(MOVY_PRE) {
            // movy_pre_<func_name>
            Some(&self.name[MOVY_PRE.len() + 1..])
        } else {
            None
        }
    }

    pub fn try_derive_movy_post(&self) -> Option<&str> {
        if self.name.starts_with(MOVY_POST) {
            // movy_post_<func_name>
            Some(&self.name[MOVY_POST.len() + 1..])
        } else {
            None
        }
    }

    pub fn is_movy_pre_ptb(&self) -> bool {
        matches!(self.try_derive_movy_pre(), Some(func_name) if func_name == MOVY_SEQUENCE)
    }

    pub fn is_movy_post_ptb(&self) -> bool {
        matches!(self.try_derive_movy_post(), Some(func_name) if func_name == MOVY_SEQUENCE)
    }
}

impl Display for MoveFunctionAbi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "{} {}{}({}){}",
            &self.visibility,
            &self.name,
            if self.type_parameters.is_empty() {
                "".to_string()
            } else {
                let tys = self
                    .type_parameters
                    .iter()
                    .enumerate()
                    .map(|(idx, v)| v.to_string_with_name(&format!("T{}", idx)))
                    .join(", ");
                format!("<{}>", tys)
            },
            self.parameters
                .iter()
                .map(|v| if f.alternate() {
                    format!("{:#}", v)
                } else {
                    v.to_string()
                })
                .join(", "),
            if self.return_paramters.is_empty() {
                "".to_string()
            } else {
                format!(
                    ": {}",
                    self.return_paramters
                        .iter()
                        .map(|v| if f.alternate() {
                            format!("{:#}", v)
                        } else {
                            v.to_string()
                        })
                        .join(", ")
                )
            }
        ))
    }
}

impl MoveFunctionAbi {
    pub(crate) fn from_module_function_handle_visibility(
        fdecl: &FunctionHandle,
        module: &CompiledModule,
        vis: MoveFunctionVisibility,
    ) -> Self {
        let ftys = fdecl
            .type_parameters
            .iter()
            .map(|v| MoveAbility::from(*v))
            .collect();
        let fname = module.identifier_at(fdecl.name).to_string();
        let fparameters = module.signature_at(fdecl.parameters);
        let freturns = module.signature_at(fdecl.return_);
        let parameters = fparameters
            .0
            .iter()
            .map(|v| MoveAbiSignatureToken::from_sui_token_module(v, &ftys, module))
            .collect();
        let returns = freturns
            .0
            .iter()
            .map(|v| MoveAbiSignatureToken::from_sui_token_module(v, &ftys, module))
            .collect();
        Self {
            name: fname,
            type_parameters: ftys,
            visibility: vis,
            parameters,
            return_paramters: returns,
        }
    }
    pub fn from_module_def(fdef: &FunctionDefinition, module: &CompiledModule) -> Self {
        let fdecl = module.function_handle_at(fdef.function);
        let vis = MoveFunctionVisibility::from(fdef.visibility);
        Self::from_module_function_handle_visibility(fdecl, module, vis)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MoveModuleId {
    pub module_address: MoveAddress,
    pub module_name: String,
}

impl MoveModuleId {
    pub fn from_module_handle(handle: &ModuleHandle, module: &CompiledModule) -> Self {
        let module_address = module.address_identifier_at(handle.address);
        let module_name = module.identifier_at(handle.name);
        Self {
            module_address: (*module_address).into(),
            module_name: module_name.to_string(),
        }
    }

    pub fn to_canonical_string(&self, prefix: bool) -> String {
        format!(
            "{}::{}",
            self.module_address.to_canonical_string(prefix),
            self.module_name
        )
    }
}

impl Display for MoveModuleId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "{}::{}",
            &self.module_address, &self.module_name
        ))
    }
}

impl FromStr for MoveModuleId {
    type Err = MovyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<_> = s.split("::").collect();
        if parts.len() != 2 {
            return Err(MovyError::InvalidIdentifier(format!(
                "Invalid MoveModuleId string: {}",
                s
            )));
        }
        let module_address = MoveAddress::from_str(parts[0])?;
        let module_name = parts[1].to_string();
        Ok(Self {
            module_address,
            module_name,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MoveModuleAbi {
    pub module_id: MoveModuleId,
    pub structs: Vec<MoveStructAbi>,
    pub functions: Vec<MoveFunctionAbi>,
}

impl MoveModuleAbi {
    pub fn locate_movy_init(&self) -> Option<&MoveFunctionAbi> {
        self.functions.iter().find(|v| v.is_movy_init())
    }
    pub fn movy_oracles(&self) -> Vec<&MoveFunctionAbi> {
        self.functions
            .iter()
            .filter(|v| v.is_movy_oracle())
            .collect()
    }
    pub fn is_test_only_module(&self) -> bool {
        self.functions.iter().any(|v| v.name == "unit_test_poison")
    }
    pub fn from_sui_module(module: &CompiledModule) -> Self {
        let module_id: MoveAddress = (*module.address()).into();
        let module_name = module.name().to_string();

        let mut structs = vec![];
        for st in module.struct_defs() {
            structs.push(MoveStructAbi::from_module_idx(st.struct_handle, module));
        }

        let mut functions = vec![];
        for f in module.function_defs() {
            functions.push(MoveFunctionAbi::from_module_def(f, module));
        }
        Self {
            module_id: MoveModuleId {
                module_address: module_id,
                module_name,
            },
            structs,
            functions,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MovePackageAbi {
    pub package_id: MoveAddress, // This might be different on sui due to upgrading
    pub modules: Vec<MoveModuleAbi>,
}

impl MovePackageAbi {
    pub fn published_at(&mut self, address: MoveAddress) {
        let prev = self.package_id;
        self.package_id = address;
        for md in self.modules.iter_mut() {
            md.module_id.module_address = address;
            for st in md.structs.iter_mut() {
                st.module_id.module_address = address;
            }
            for fc in md.functions.iter_mut() {
                for ty in fc.parameters.iter_mut() {
                    ty.published_at(prev, address);
                }
                for ty in fc.return_paramters.iter_mut() {
                    ty.published_at(prev, address);
                }
            }
        }
    }
    pub fn from_sui_id_and_modules<'a>(
        id: ObjectID,
        modules: impl Iterator<Item = &'a CompiledModule>,
    ) -> Result<Self, MovyError> {
        let mut out_modules = vec![];
        for module in modules {
            let module_abi = MoveModuleAbi::from_sui_module(module);
            out_modules.push(module_abi);
        }
        Ok(Self {
            package_id: id.into(),
            modules: out_modules,
        })
    }
    pub fn from_sui_package(pkg: &sui_types::move_package::MovePackage) -> Result<Self, MovyError> {
        let id = pkg.id();
        let modules = pkg
            .serialized_module_map()
            .iter()
            .map(|v| CompiledModule::deserialize_with_defaults(v.1))
            .collect::<Result<Vec<_>, _>>()?;
        Self::from_sui_id_and_modules(id, modules.iter())
    }
    pub fn from_sui_object(object: &Object) -> Result<Self, MovyError> {
        Self::from_sui_package(
            object
                .data
                .try_as_package()
                .ok_or_else(|| eyre!("expect package"))?,
        )
    }
}
